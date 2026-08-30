//! One config value, the whole persistence stack.
//!
//! The ledger and the vault are chosen the same way, from the same setting, so
//! every host was writing the same two `match`es — and scoping the two handles
//! separately, which is the leak waiting to happen: a request that calls
//! `for_tenant` on the ledger and forgets the vault has isolated the learning
//! and shared the graphs.
//!
//! [`Storage::open`] does the picking; [`Storage::for_tenant`] scopes **both
//! halves in one call**, so there is nothing to forget.
//!
//! ```text
//! "memory"                          → forgets on restart; tests, first look
//! "adaptive.db"  or  "sqlite:…"     → one SQLite file holding BOTH halves
//! "mongodb://host/db"               → one Mongo database holding both
//! ```
//!
//! A URI for a backend this build does not carry fails **at parse time**, with
//! the feature named — a config error at boot, not a missing symbol at the
//! first write.

use std::path::PathBuf;

use async_trait::async_trait;
use tinyflows::store::types::{WorkflowError, WorkflowRecord};

use crate::execute::StepRecord;
use crate::ledger::memory::MemoryLedger;
use crate::ledger::{
    Episode, Ledger, LedgerRow, Lesson, LessonKind, Page, Result as LedgerResult, Score,
};
use crate::workflows::Vault;
use crate::workflows::memory::MemoryVault;

/// What went wrong turning a config value into storage.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// The value does not name a storage this build can open.
    #[error("storage config: {0}")]
    Config(String),
    /// The ledger backend refused to open.
    #[error("ledger: {0}")]
    Ledger(#[from] crate::ledger::LedgerError),
    /// The vault backend refused to open.
    #[error("vault: {0}")]
    Vault(#[from] WorkflowError),
}

/// Where the storage setting is read from, when the environment supplies it.
pub const STORAGE_VAR: &str = "TINYFLOWS_ADAPTIVE_STORAGE";

/// Where everything durable goes, parsed from one setting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Config {
    /// In memory; forgets on restart. Never picked implicitly — the value has
    /// to literally say `memory`.
    Memory,
    /// One SQLite file, holding the ledger and the vault side by side.
    #[cfg(feature = "sqlite")]
    Sqlite(PathBuf),
    /// One MongoDB database, holding both.
    #[cfg(feature = "mongo")]
    Mongo {
        /// The connection string, passed to the driver untouched.
        uri: String,
        /// The database name, taken from the URI's path or defaulted.
        database: String,
    },
}

impl Config {
    /// Read a storage setting.
    ///
    /// * `memory` (or `:memory:`) — the ledger and vault that forget;
    /// * `mongodb://…` / `mongodb+srv://…` — Mongo, database from the URI's
    ///   first path segment, `tinyflows_adaptive` when it has none;
    /// * `sqlite:<path>` — SQLite at that path;
    /// * anything else — treated as a filesystem path, SQLite.
    ///
    /// Read the setting from the environment: [`STORAGE_VAR`].
    ///
    /// An unset variable is an **error naming the variable**, never a default.
    /// The tempting fallbacks are both wrong: defaulting to a disk location
    /// invents a path on the operator's machine nobody named, and defaulting
    /// to memory is a service that runs perfectly and learns nothing — the
    /// failure shape this crate is built to refuse.
    ///
    /// # Errors
    /// When the variable is unset, or its value fails [`parse`](Self::parse).
    pub fn from_env() -> Result<Self, StorageError> {
        Self::from_setting(std::env::var(STORAGE_VAR).ok().as_deref())
    }

    /// [`from_env`](Self::from_env) with the read made explicit, so the rule is
    /// testable without any test mutating process-wide state.
    pub fn from_setting(value: Option<&str>) -> Result<Self, StorageError> {
        match value.map(str::trim).filter(|v| !v.is_empty()) {
            Some(value) => Self::parse(value),
            None => Err(StorageError::Config(format!(
                "{STORAGE_VAR} is not set; expected `memory`, a sqlite path, or a mongodb:// URI"
            ))),
        }
    }

    /// # Errors
    /// When the value names a backend this build was compiled without — caught
    /// here so it fails at boot with the feature named, not at first use.
    pub fn parse(value: &str) -> Result<Self, StorageError> {
        let value = value.trim();
        if value.is_empty() {
            return Err(StorageError::Config(
                "empty storage setting; expected `memory`, a sqlite path, or a mongodb:// URI"
                    .to_string(),
            ));
        }
        if value == "memory" || value == ":memory:" {
            return Ok(Self::Memory);
        }
        if value.starts_with("mongodb://") || value.starts_with("mongodb+srv://") {
            #[cfg(feature = "mongo")]
            return Ok(Self::Mongo {
                uri: value.to_string(),
                database: mongo_database(value),
            });
            #[cfg(not(feature = "mongo"))]
            return Err(StorageError::Config(
                "a mongodb:// URI, but this build has no `mongo` feature".to_string(),
            ));
        }
        let path = value.strip_prefix("sqlite:").unwrap_or(value);
        #[cfg(feature = "sqlite")]
        return Ok(Self::Sqlite(PathBuf::from(path)));
        #[cfg(not(feature = "sqlite"))]
        {
            let _ = path;
            Err(StorageError::Config(format!(
                "`{value}` reads as a sqlite path, but this build has no `sqlite` feature"
            )))
        }
    }
}

/// The database named by a Mongo URI's first path segment, or the default.
#[cfg(feature = "mongo")]
fn mongo_database(uri: &str) -> String {
    let after_scheme = uri.split_once("://").map_or(uri, |(_, rest)| rest);
    let path = after_scheme.split_once('/').map(|(_, path)| path);
    let database = path
        .map(|p| p.split(['?', '/']).next().unwrap_or(""))
        .unwrap_or("");
    if database.is_empty() {
        "tinyflows_adaptive".to_string()
    } else {
        database.to_string()
    }
}

/// A ledger and a vault, opened from one [`Config`] and scoped together.
pub struct Storage {
    ledger: AnyLedger,
    vault: AnyVault,
}

impl Storage {
    /// [`Config::from_env`] and [`open`](Self::open) in one call — the whole
    /// persistence stack from the environment.
    ///
    /// # Errors
    /// As both halves.
    pub async fn from_env() -> Result<Self, StorageError> {
        Self::open(&Config::from_env()?).await
    }

    /// Open both halves of the configured backend.
    ///
    /// SQLite puts them in **one file** — their schemas share no table — so a
    /// single-node deployment backs up exactly one thing. Mongo puts them in
    /// one database, in separate collections.
    ///
    /// # Errors
    /// When the backend cannot be opened or reached.
    pub async fn open(config: &Config) -> Result<Self, StorageError> {
        Ok(match config {
            Config::Memory => Self {
                ledger: AnyLedger::Memory(MemoryLedger::new()),
                vault: AnyVault::Memory(MemoryVault::new()),
            },
            #[cfg(feature = "sqlite")]
            Config::Sqlite(path) => Self {
                ledger: AnyLedger::Sqlite(crate::ledger::sqlite::SqliteLedger::open(path)?),
                vault: AnyVault::Sqlite(crate::workflows::sqlite::SqliteVault::open(path)?),
            },
            #[cfg(feature = "mongo")]
            Config::Mongo { uri, database } => Self {
                ledger: AnyLedger::Mongo(
                    crate::ledger::mongo::MongoLedger::connect(uri, database).await?,
                ),
                vault: AnyVault::Mongo(
                    crate::workflows::mongo::MongoVault::connect(uri, database).await?,
                ),
            },
        })
    }

    /// Both halves, scoped to one tenant, in one call.
    ///
    /// One call rather than two because the failure this module exists to
    /// prevent is scoping the ledger and forgetting the vault — isolated
    /// learning over shared graphs, or the reverse.
    #[must_use]
    pub fn for_tenant(&self, scope: &str) -> Self {
        Self {
            ledger: self.ledger.for_tenant(scope),
            vault: self.vault.for_tenant(scope),
        }
    }

    /// The ledger half.
    #[must_use]
    pub fn ledger(&self) -> &AnyLedger {
        &self.ledger
    }

    /// The vault half.
    #[must_use]
    pub fn vault(&self) -> &AnyVault {
        &self.vault
    }
}

/// Whichever ledger the config picked, behind the one trait.
pub enum AnyLedger {
    /// Forgets on restart.
    Memory(MemoryLedger),
    /// One SQLite file.
    #[cfg(feature = "sqlite")]
    Sqlite(crate::ledger::sqlite::SqliteLedger),
    /// A MongoDB database.
    #[cfg(feature = "mongo")]
    Mongo(crate::ledger::mongo::MongoLedger),
}

impl AnyLedger {
    /// A handle onto the same store, scoped to one tenant.
    #[must_use]
    pub fn for_tenant(&self, scope: &str) -> Self {
        match self {
            Self::Memory(l) => Self::Memory(l.for_tenant(scope)),
            #[cfg(feature = "sqlite")]
            Self::Sqlite(l) => Self::Sqlite(l.for_tenant(scope)),
            #[cfg(feature = "mongo")]
            Self::Mongo(l) => Self::Mongo(l.for_tenant(scope)),
        }
    }
}

/// Delegate one call to whichever backend is inside.
macro_rules! on_ledger {
    ($self:ident, $l:ident => $call:expr) => {
        match $self {
            AnyLedger::Memory($l) => $call,
            #[cfg(feature = "sqlite")]
            AnyLedger::Sqlite($l) => $call,
            #[cfg(feature = "mongo")]
            AnyLedger::Mongo($l) => $call,
        }
    };
}

#[async_trait]
impl Ledger for AnyLedger {
    fn scope(&self) -> Option<&str> {
        on_ledger!(self, l => l.scope())
    }
    async fn append(&self, row: &LedgerRow) -> LedgerResult<String> {
        on_ledger!(self, l => l.append(row).await)
    }
    async fn rows(&self, episode: &str) -> LedgerResult<Vec<LedgerRow>> {
        on_ledger!(self, l => l.rows(episode).await)
    }
    async fn promote(&self, lesson: &Lesson, cites: &[String]) -> LedgerResult<String> {
        on_ledger!(self, l => l.promote(lesson, cites).await)
    }
    async fn lessons(&self, kind: Option<LessonKind>) -> LedgerResult<Vec<Lesson>> {
        on_ledger!(self, l => l.lessons(kind).await)
    }
    async fn evidence(&self, lesson_id: &str) -> LedgerResult<Vec<LedgerRow>> {
        on_ledger!(self, l => l.evidence(lesson_id).await)
    }
    async fn score_lesson(&self, lesson_id: &str, helped: bool) -> LedgerResult<()> {
        on_ledger!(self, l => l.score_lesson(lesson_id, helped).await)
    }
    async fn score_workflow(&self, workflow_id: &str, helped: bool) -> LedgerResult<()> {
        on_ledger!(self, l => l.score_workflow(workflow_id, helped).await)
    }
    async fn workflow_score(&self, workflow_id: &str) -> LedgerResult<Score> {
        on_ledger!(self, l => l.workflow_score(workflow_id).await)
    }
    async fn link_variant(&self, parent: &str, variant: &str) -> LedgerResult<()> {
        on_ledger!(self, l => l.link_variant(parent, variant).await)
    }
    async fn parent_of(&self, id: &str) -> LedgerResult<Option<String>> {
        on_ledger!(self, l => l.parent_of(id).await)
    }
    async fn children_of(&self, id: &str) -> LedgerResult<Vec<String>> {
        on_ledger!(self, l => l.children_of(id).await)
    }
    async fn save_episode(&self, episode: &Episode) -> LedgerResult<()> {
        on_ledger!(self, l => l.save_episode(episode).await)
    }
    async fn episode(&self, id: &str) -> LedgerResult<Option<Episode>> {
        on_ledger!(self, l => l.episode(id).await)
    }
    async fn episodes(&self, running_only: bool, page: Page) -> LedgerResult<Vec<Episode>> {
        on_ledger!(self, l => l.episodes(running_only, page).await)
    }
    async fn save_steps(&self, row_id: &str, steps: &[StepRecord]) -> LedgerResult<()> {
        on_ledger!(self, l => l.save_steps(row_id, steps).await)
    }
    async fn steps(&self, row_id: &str) -> LedgerResult<Vec<StepRecord>> {
        on_ledger!(self, l => l.steps(row_id).await)
    }
}

/// Whichever vault the config picked, behind the one trait.
pub enum AnyVault {
    /// Forgets on restart.
    Memory(MemoryVault),
    /// One SQLite file — the same one the ledger may use.
    #[cfg(feature = "sqlite")]
    Sqlite(crate::workflows::sqlite::SqliteVault),
    /// A MongoDB database.
    #[cfg(feature = "mongo")]
    Mongo(crate::workflows::mongo::MongoVault),
}

impl AnyVault {
    /// A handle onto the same store, scoped to one tenant.
    #[must_use]
    pub fn for_tenant(&self, scope: &str) -> Self {
        match self {
            Self::Memory(v) => Self::Memory(v.for_tenant(scope)),
            #[cfg(feature = "sqlite")]
            Self::Sqlite(v) => Self::Sqlite(v.for_tenant(scope)),
            #[cfg(feature = "mongo")]
            Self::Mongo(v) => Self::Mongo(v.for_tenant(scope)),
        }
    }
}

macro_rules! on_vault {
    ($self:ident, $v:ident => $call:expr) => {
        match $self {
            AnyVault::Memory($v) => $call,
            #[cfg(feature = "sqlite")]
            AnyVault::Sqlite($v) => $call,
            #[cfg(feature = "mongo")]
            AnyVault::Mongo($v) => $call,
        }
    };
}

#[async_trait]
impl Vault for AnyVault {
    fn scope(&self) -> Option<&str> {
        on_vault!(self, v => v.scope())
    }
    async fn load(&self) -> Result<Vec<WorkflowRecord>, WorkflowError> {
        on_vault!(self, v => v.load().await)
    }
    async fn put(&self, record: &WorkflowRecord) -> Result<(), WorkflowError> {
        on_vault!(self, v => v.put(record).await)
    }
    async fn remove(&self, id: &str) -> Result<(), WorkflowError> {
        on_vault!(self, v => v.remove(id).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_has_to_be_asked_for_by_name() {
        assert_eq!(Config::parse("memory").expect("parse"), Config::Memory);
        assert_eq!(Config::parse(":memory:").expect("parse"), Config::Memory);
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn a_bare_path_reads_as_sqlite() {
        assert_eq!(
            Config::parse("/var/lib/app/adaptive.db").expect("parse"),
            Config::Sqlite(PathBuf::from("/var/lib/app/adaptive.db"))
        );
        assert_eq!(
            Config::parse("sqlite:./adaptive.db").expect("parse"),
            Config::Sqlite(PathBuf::from("./adaptive.db"))
        );
    }

    #[cfg(feature = "mongo")]
    #[test]
    fn a_mongo_uri_carries_its_database_or_gets_the_default() {
        match Config::parse("mongodb://db.internal:27017/adaptive?replicaSet=rs0").expect("parse") {
            Config::Mongo { database, .. } => assert_eq!(database, "adaptive"),
            other => panic!("{other:?}"),
        }
        match Config::parse("mongodb+srv://cluster.example.net").expect("parse") {
            Config::Mongo { database, .. } => assert_eq!(database, "tinyflows_adaptive"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_unset_variable_errors_naming_the_variable_rather_than_defaulting() {
        // Defaulting to a path invents a location nobody named; defaulting to
        // memory is a service that runs perfectly and learns nothing.
        let err = Config::from_setting(None).expect_err("unset");
        assert!(err.to_string().contains(STORAGE_VAR), "{err}");
        let err = Config::from_setting(Some("  ")).expect_err("blank is unset");
        assert!(err.to_string().contains(STORAGE_VAR), "{err}");
    }

    #[test]
    fn a_set_variable_goes_through_the_same_parse() {
        assert_eq!(
            Config::from_setting(Some("memory")).expect("parse"),
            Config::Memory
        );
    }

    #[test]
    fn an_empty_setting_is_an_error_that_lists_the_choices() {
        let err = Config::parse("   ").expect_err("empty");
        assert!(err.to_string().contains("memory"), "{err}");
    }

    #[tokio::test]
    async fn one_call_scopes_both_halves() {
        // The failure this module exists to prevent: scoping the ledger and
        // forgetting the vault, or the reverse.
        let storage = Storage::open(&Config::Memory).await.expect("open");
        let tenant = storage.for_tenant("user-7");
        assert_eq!(tenant.ledger().scope(), Some("user-7"));
        assert_eq!(tenant.vault().scope(), Some("user-7"));
        assert_eq!(storage.ledger().scope(), None, "the root stays unscoped");
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn one_sqlite_setting_yields_one_file_holding_both_halves() {
        let dir = std::env::temp_dir().join(format!("adaptive-storage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("adaptive.db");

        let config = Config::parse(path.to_str().expect("utf8 path")).expect("parse");
        let storage = Storage::open(&config).await.expect("open");
        let tenant = storage.for_tenant("user-7");

        tenant
            .ledger()
            .append(&crate::ledger::conformance::row("ep-1", 1, "authored"))
            .await
            .expect("append");
        tenant
            .vault()
            .put(&crate::workflows::conformance::record("weekly"))
            .await
            .expect("put");

        // Reopen from the same setting: both halves are still there, scoped.
        let again = Storage::open(&config).await.expect("reopen");
        let tenant = again.for_tenant("user-7");
        assert_eq!(tenant.ledger().rows("ep-1").await.expect("rows").len(), 1);
        assert_eq!(tenant.vault().load().await.expect("load").len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
