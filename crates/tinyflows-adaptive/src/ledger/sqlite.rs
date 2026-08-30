//! A [`Ledger`] on sqlite, for a single-process deployment.
//!
//! Synchronous work behind an async trait, on purpose. Every call here is one
//! or two short statements against a local file; wrapping them in
//! `spawn_blocking` would add a thread hop and a tokio dependency to save
//! microseconds nobody can measure. If a deployment ever puts this behind
//! enough concurrency for the lock to matter, that is the moment to move —
//! not before, and the trait means the move costs one file.

use std::sync::Mutex;

use async_trait::async_trait;
use rusqlite::{Connection, OptionalExtension, params};

use super::{
    Episode, EpisodeStatus, Ledger, LedgerError, LedgerRow, Lesson, LessonKind, Result, Score,
};

impl From<rusqlite::Error> for LedgerError {
    fn from(err: rusqlite::Error) -> Self {
        Self::Backend(err.to_string())
    }
}

/// The schema, applied on open.
///
/// `IF NOT EXISTS` throughout so opening an existing ledger is a no-op, and
/// every table carries its own id rather than relying on rowid — a row id
/// leaves this process (a lesson cites them) and rowid is not stable across a
/// vacuum.
const DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS ledger_rows (
        id            TEXT PRIMARY KEY,
        episode       TEXT NOT NULL,
        attempt       INTEGER NOT NULL,
        approach_sig  TEXT NOT NULL,
        approach_desc TEXT NOT NULL DEFAULT '',
        workflow_id   TEXT,
        outcome       TEXT NOT NULL DEFAULT '',
        cause         TEXT NOT NULL DEFAULT '',
        cost_usd      REAL NOT NULL DEFAULT 0,
        at            TEXT NOT NULL,
        satisfied     INTEGER NOT NULL DEFAULT 0,
        advanced      INTEGER NOT NULL DEFAULT 0,
        scope_key     TEXT NOT NULL DEFAULT '',
        seq           INTEGER NOT NULL
    )",
    // Ordered by `seq`, not by `at`: two attempts finishing in the same second
    // are common, and a timestamp tie makes the ledger read in an arbitrary
    // order — which silently reorders the exclusion list.
    "CREATE INDEX IF NOT EXISTS ix_rows_episode ON ledger_rows(episode, seq)",
    // `scope_key` is NOT NULL with '' for global rather than nullable: it is
    // part of the workflow-scores primary key, and SQLite does not treat two
    // NULLs as equal, so a nullable column there would let every global score
    // insert a fresh row instead of upserting the same one.
    "CREATE TABLE IF NOT EXISTS lessons (
        id        TEXT PRIMARY KEY,
        kind      TEXT NOT NULL,
        trigger   TEXT NOT NULL,
        mechanism TEXT NOT NULL DEFAULT '',
        claim     TEXT NOT NULL,
        applied   INTEGER NOT NULL DEFAULT 0,
        helped    INTEGER NOT NULL DEFAULT 0,
        scope_key TEXT NOT NULL DEFAULT '',
        seq       INTEGER NOT NULL
    )",
    "CREATE INDEX IF NOT EXISTS ix_lessons_scope ON lessons(scope_key, seq)",
    "CREATE TABLE IF NOT EXISTS lesson_evidence (
        lesson_id TEXT NOT NULL,
        row_id    TEXT NOT NULL,
        PRIMARY KEY (lesson_id, row_id)
    )",
    "CREATE TABLE IF NOT EXISTS workflow_scores (
        scope_key   TEXT NOT NULL DEFAULT '',
        workflow_id TEXT NOT NULL,
        applied     INTEGER NOT NULL DEFAULT 0,
        helped      INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (scope_key, workflow_id)
    )",
    "CREATE TABLE IF NOT EXISTS variants (
        scope_key TEXT NOT NULL DEFAULT '',
        variant   TEXT NOT NULL,
        parent    TEXT NOT NULL,
        PRIMARY KEY (scope_key, variant)
    )",
    "CREATE INDEX IF NOT EXISTS ix_variants_parent ON variants(scope_key, parent)",
    "CREATE TABLE IF NOT EXISTS episodes (
        id         TEXT NOT NULL,
        scope_key  TEXT NOT NULL DEFAULT '',
        goal       TEXT NOT NULL,
        status     TEXT NOT NULL,
        attempt    INTEGER NOT NULL DEFAULT 0,
        stalled    INTEGER NOT NULL DEFAULT 0,
        started_at TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        PRIMARY KEY (scope_key, id)
    )",
    "CREATE INDEX IF NOT EXISTS ix_episodes_scope ON episodes(scope_key, updated_at)",
    // One row per step, never one blob per attempt: a looped node produces a
    // step per iteration, and at RECORD_BUDGET that reaches past what a Mongo
    // document may hold. Uniform across backends beats convenient on one.
    "CREATE TABLE IF NOT EXISTS attempt_steps (
        scope_key     TEXT NOT NULL DEFAULT '',
        row_id        TEXT NOT NULL,
        seq           INTEGER NOT NULL,
        node_id       TEXT NOT NULL,
        status        TEXT NOT NULL,
        output        TEXT NOT NULL,
        duration_ms   INTEGER NOT NULL DEFAULT 0,
        null_bindings TEXT NOT NULL DEFAULT '[]',
        transcript    TEXT NOT NULL DEFAULT '[]',
        PRIMARY KEY (scope_key, row_id, seq)
    )",
];

/// Applied after [`DDL`], failures ignored.
///
/// A ledger written before scoping existed has the columns missing rather than
/// empty, and `CREATE TABLE IF NOT EXISTS` will not add them. `ADD COLUMN`
/// errors once the column is there, which is the expected case on every start
/// after the first — so these are the statements whose failure means success.
const MIGRATIONS: &[&str] = &[
    "ALTER TABLE attempt_steps ADD COLUMN transcript TEXT NOT NULL DEFAULT '[]'",
    "ALTER TABLE lessons ADD COLUMN scope_key TEXT NOT NULL DEFAULT ''",
    "ALTER TABLE workflow_scores ADD COLUMN scope_key TEXT NOT NULL DEFAULT ''",
    "ALTER TABLE ledger_rows ADD COLUMN satisfied INTEGER NOT NULL DEFAULT 0",
    "ALTER TABLE ledger_rows ADD COLUMN advanced INTEGER NOT NULL DEFAULT 0",
    "ALTER TABLE ledger_rows ADD COLUMN scope_key TEXT NOT NULL DEFAULT ''",
];

/// Where the ledger lives, when the environment says.
pub const DB_PATH_VAR: &str = "TINYFLOWS_ADAPTIVE_DB";

/// Where a platform keeps application **data**.
///
/// Data, not cache and not config. A ledger is not regenerable, so a cache
/// sweeper finding it would delete everything the loop has learned; and it is
/// not something a person edits, so a config directory would invite exactly
/// that. Every platform below distinguishes the three, and this picks the one
/// whose contract is "keep this".
///
/// Taken as a parameter rather than read from `cfg!` so all three rules are
/// tested on whichever machine runs the suite. A rule that only compiles on the
/// platform it is wrong for is a rule nobody checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    /// XDG Base Directory Specification.
    Xdg,
    /// Apple's File System Programming Guide.
    MacOs,
    /// Windows known folders.
    Windows,
}

impl Platform {
    /// What this build is running on.
    #[must_use]
    pub fn host() -> Self {
        if cfg!(target_os = "macos") {
            Self::MacOs
        } else if cfg!(windows) {
            Self::Windows
        } else {
            Self::Xdg
        }
    }
}

/// The documented data directory, or `None` when the environment does not say.
///
/// * **XDG** — `$XDG_DATA_HOME`, else `$HOME/.local/share`. The spec names that
///   fallback, so an unset variable is normal rather than a failure.
/// * **macOS** — `$HOME/Library/Application Support`.
/// * **Windows** — `%LOCALAPPDATA%`, **not** `%APPDATA%`. The roaming profile
///   syncs between machines, and a SQLite file copied mid-write between two
///   machines that both think they own it is a corrupted database. Local is the
///   right shelf for anything a process holds open.
///
/// `None` is a real answer: a daemon under a user with no home has nowhere by
/// convention, and inventing one would put a database somewhere nobody looks.
fn data_dir(
    platform: Platform,
    env: &dyn Fn(&str) -> Option<String>,
) -> Option<std::path::PathBuf> {
    let read = |key: &str| env(key).filter(|v| !v.trim().is_empty());
    match platform {
        Platform::Xdg => read("XDG_DATA_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| read("HOME").map(|h| std::path::PathBuf::from(h).join(".local/share"))),
        Platform::MacOs => {
            read("HOME").map(|h| std::path::PathBuf::from(h).join("Library/Application Support"))
        }
        Platform::Windows => read("LOCALAPPDATA").map(std::path::PathBuf::from),
    }
}

/// The directory this project owns inside the platform's data directory.
///
/// Named for the project, not this crate, so a sibling shares the folder rather
/// than scattering one per crate across a user's disk.
const APP_DIR: &str = "tinyflows";

/// The file, inside that.
const DB_FILE: &str = "adaptive.db";

/// Which path wins: the environment when it names one, the caller otherwise.
///
/// Pulled out as a pure function so the rule is tested without any test setting
/// a process-wide variable — `unsafe_code` is forbidden here, and an env-mutating
/// test is a test that fails when another one runs beside it.
///
/// Blank and whitespace-only are treated as unset: an empty variable is what a
/// shell leaves behind when a value was meant to be interpolated and was not,
/// and opening `""` fails in a way that names nothing useful.
fn chosen_path(configured: Option<&str>, fallback: &std::path::Path) -> std::path::PathBuf {
    match configured.map(str::trim).filter(|p| !p.is_empty()) {
        Some(path) => std::path::PathBuf::from(path),
        None => fallback.to_path_buf(),
    }
}

/// The conventional path, or an error naming the way out.
fn default_path(
    platform: Platform,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<std::path::PathBuf> {
    if let Some(configured) = env(DB_PATH_VAR).filter(|v| !v.trim().is_empty()) {
        return Ok(std::path::PathBuf::from(configured.trim()));
    }
    data_dir(platform, env)
        .map(|dir| dir.join(APP_DIR).join(DB_FILE))
        .ok_or_else(|| {
            LedgerError::Backend(format!(
                "no data directory on this platform; set {DB_PATH_VAR} to a writable path"
            ))
        })
}

/// A ledger backed by one sqlite file.
pub struct SqliteLedger {
    conn: std::sync::Arc<Mutex<Connection>>,
    scope: Option<String>,
}

impl SqliteLedger {
    /// Open (or create) a ledger at `path`.
    ///
    /// # Errors
    /// When the file cannot be opened or the schema cannot be applied.
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let path = path.as_ref();
        // Create the parent, because `Connection::open` creates the file and
        // not the directory holding it. Every sensible location for a ledger —
        // `~/.config/something/`, `/var/lib/something/`, a data volume — is a
        // directory that may not exist on a first run, and failing there reads
        // as "the database is broken" rather than "make the folder".
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .map_err(|e| LedgerError::Backend(format!("{}: {e}", parent.display())))?;
        }
        Self::from_connection(Connection::open(path)?)
    }

    /// Open the path in `TINYFLOWS_ADAPTIVE_DB`, or `fallback` when it is unset.
    ///
    /// The library does not invent a location on your disk. A crate that writes
    /// to a home directory nobody named is a crate that surprises an operator
    /// once and is distrusted afterwards, and the right place differs entirely
    /// between a CLI, a container and a service with a mounted volume.
    ///
    /// So the fallback stays visible in your code and the environment can move
    /// it without a rebuild — which is what a deployment actually needs. Either
    /// way the parent directory is created.
    ///
    /// # Errors
    /// As [`open`](Self::open).
    pub fn from_env_or(fallback: impl AsRef<std::path::Path>) -> Result<Self> {
        let configured = std::env::var(DB_PATH_VAR).ok();
        Self::open(chosen_path(configured.as_deref(), fallback.as_ref()))
    }

    /// Open the ledger where this platform keeps application data.
    ///
    /// `TINYFLOWS_ADAPTIVE_DB` still wins when it is set. Otherwise:
    ///
    /// | Platform | Path |
    /// |---|---|
    /// | Linux and other XDG | `$XDG_DATA_HOME/tinyflows/adaptive.db`, else `~/.local/share/tinyflows/adaptive.db` |
    /// | macOS | `~/Library/Application Support/tinyflows/adaptive.db` |
    /// | Windows | `%LOCALAPPDATA%\tinyflows\adaptive.db` |
    ///
    /// Right for a CLI or a desktop agent, which is what a convention is for.
    /// A container or a service should name its own path — a volume mount is
    /// the whole point, and a convention that lands the database inside an
    /// ephemeral layer is worse than no convention.
    ///
    /// # Errors
    /// When the platform's data directory cannot be determined — a daemon under
    /// a user with no home has nowhere by convention, and the error says to set
    /// the variable rather than guessing somewhere nobody looks. Also as
    /// [`open`](Self::open).
    pub fn at_default_location() -> Result<Self> {
        Self::open(default_path(Platform::host(), &|key| {
            std::env::var(key).ok()
        })?)
    }

    /// A ledger held entirely in memory. For tests, and for a host that wants
    /// the loop to run without learning anything durable.
    ///
    /// # Errors
    /// When the schema cannot be applied.
    pub fn in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA busy_timeout = 5000;")
            .ok();
        for statement in DDL {
            conn.execute(statement, [])?;
        }
        for statement in MIGRATIONS {
            let _ = conn.execute(statement, []);
        }
        migrate_episode_identity(&conn)?;
        Ok(Self {
            conn: std::sync::Arc::new(Mutex::new(conn)),
            scope: None,
        })
    }

    /// A handle onto the same database, scoped to one tenant.
    ///
    /// Cheap — it shares the connection. Construct one per request at the edge
    /// of the service and hand it to the loop; everything downstream reads and
    /// writes the right bucket without knowing a tenant exists.
    #[must_use]
    pub fn for_tenant(&self, scope: impl Into<String>) -> Self {
        Self {
            conn: std::sync::Arc::clone(&self.conn),
            scope: Some(scope.into()),
        }
    }

    /// This handle's bucket, as stored: `''` for global.
    fn bucket(&self) -> &str {
        self.scope.as_deref().unwrap_or_default()
    }

    fn guard(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        // A poisoned lock means a previous caller panicked mid-write. The
        // ledger is append-mostly and every write is a single statement, so
        // the data is intact; refusing every later call would turn one panic
        // into a dead loop.
        Ok(self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()))
    }
}

/// Rebuild the pre-tenancy episodes table whose primary key was only `id`.
fn migrate_episode_identity(conn: &Connection) -> Result<()> {
    let scoped_primary_key: i64 = conn.query_row(
        "SELECT pk FROM pragma_table_info('episodes') WHERE name = 'scope_key'",
        [],
        |row| row.get(0),
    )?;
    if scoped_primary_key != 0 {
        return Ok(());
    }
    conn.execute_batch(
        "BEGIN IMMEDIATE;
         CREATE TABLE episodes_scoped (
            id TEXT NOT NULL,
            scope_key TEXT NOT NULL DEFAULT '',
            goal TEXT NOT NULL,
            status TEXT NOT NULL,
            attempt INTEGER NOT NULL DEFAULT 0,
            stalled INTEGER NOT NULL DEFAULT 0,
            started_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            PRIMARY KEY (scope_key, id)
         );
         INSERT INTO episodes_scoped
            SELECT id, scope_key, goal, status, attempt, stalled, started_at, updated_at
            FROM episodes;
         DROP TABLE episodes;
         ALTER TABLE episodes_scoped RENAME TO episodes;
         CREATE INDEX ix_episodes_scope ON episodes(scope_key, updated_at);
         COMMIT;",
    )?;
    Ok(())
}

fn next_seq(conn: &Connection, table: &str) -> Result<i64> {
    let current: Option<i64> = conn
        .query_row(&format!("SELECT MAX(seq) FROM {table}"), [], |r| r.get(0))
        .optional()?
        .flatten();
    Ok(current.unwrap_or(0) + 1)
}

fn new_id(prefix: &str, seq: i64) -> String {
    format!("{prefix}_{seq:08}")
}

fn read_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<LedgerRow> {
    Ok(LedgerRow {
        id: r.get("id")?,
        episode: r.get("episode")?,
        attempt: r.get::<_, i64>("attempt")?.try_into().unwrap_or(0),
        approach_sig: r.get("approach_sig")?,
        approach_desc: r.get("approach_desc")?,
        workflow_id: r.get("workflow_id")?,
        outcome: r.get("outcome")?,
        cause: r.get("cause")?,
        cost_usd: r.get("cost_usd")?,
        at: r.get("at")?,
        satisfied: r.get::<_, i64>("satisfied")? != 0,
        advanced: r.get::<_, i64>("advanced")? != 0,
    })
}

/// Read an episode row, deferring the JSON columns' failure to the caller.
///
/// The inner `Result` is deliberate: `query_map` cannot carry a
/// [`LedgerError`], and swallowing a goal that no longer parses would hand the
/// loop an empty goal and let it run against nothing.
fn read_episode(r: &rusqlite::Row<'_>) -> rusqlite::Result<Result<Episode>> {
    let goal: String = r.get("goal")?;
    let status: String = r.get("status")?;
    let scope: String = r.get("scope_key")?;
    Ok((|| {
        Ok(Episode {
            id: r.get("id").unwrap_or_default(),
            goal: serde_json::from_str(&goal).map_err(|e| LedgerError::Corrupt(e.to_string()))?,
            scope_key: (!scope.is_empty()).then_some(scope),
            status: serde_json::from_str(&status)
                .map_err(|e| LedgerError::Corrupt(e.to_string()))?,
            attempt: r
                .get::<_, i64>("attempt")
                .unwrap_or(0)
                .try_into()
                .unwrap_or(0),
            stalled: r
                .get::<_, i64>("stalled")
                .unwrap_or(0)
                .try_into()
                .unwrap_or(0),
            started_at: r.get("started_at").unwrap_or_default(),
            updated_at: r.get("updated_at").unwrap_or_default(),
        })
    })())
}

include!("sqlite/ledger_impl.rs");
#[cfg(test)]
#[path = "sqlite_tests.rs"]
mod tests;
