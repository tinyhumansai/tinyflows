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
        id         TEXT PRIMARY KEY,
        scope_key  TEXT NOT NULL DEFAULT '',
        goal       TEXT NOT NULL,
        status     TEXT NOT NULL,
        attempt    INTEGER NOT NULL DEFAULT 0,
        stalled    INTEGER NOT NULL DEFAULT 0,
        started_at TEXT NOT NULL,
        updated_at TEXT NOT NULL
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

#[async_trait]
impl Ledger for SqliteLedger {
    fn scope(&self) -> Option<&str> {
        self.scope.as_deref()
    }

    async fn append(&self, row: &LedgerRow) -> Result<String> {
        let conn = self.guard()?;
        let seq = next_seq(&conn, "ledger_rows")?;
        let id = new_id("ldg", seq);
        conn.execute(
            "INSERT INTO ledger_rows(id, episode, attempt, approach_sig, approach_desc,
                                     workflow_id, outcome, cause, cost_usd, at,
                                     satisfied, advanced, scope_key, seq)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            params![
                id,
                row.episode,
                i64::from(row.attempt),
                row.approach_sig,
                row.approach_desc,
                row.workflow_id,
                row.outcome,
                row.cause,
                row.cost_usd,
                row.at,
                i64::from(row.satisfied),
                i64::from(row.advanced),
                self.bucket(),
                seq,
            ],
        )?;
        Ok(id)
    }

    async fn rows(&self, episode: &str) -> Result<Vec<LedgerRow>> {
        let conn = self.guard()?;
        // Scoped as well as keyed by episode. An episode id is opaque and a
        // service may hand one straight through from a request path, so this
        // must not be the one read where guessing an id is enough.
        let mut stmt = conn.prepare(
            "SELECT * FROM ledger_rows WHERE episode = ?1 AND scope_key = ?2 ORDER BY seq",
        )?;
        let found = stmt
            .query_map(params![episode, self.bucket()], read_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(found)
    }

    async fn promote(&self, lesson: &Lesson, cites: &[String]) -> Result<String> {
        let conn = self.guard()?;
        let seq = next_seq(&conn, "lessons")?;
        let id = new_id("les", seq);
        conn.execute(
            "INSERT INTO lessons(id, kind, trigger, mechanism, claim, applied, helped,
                                 scope_key, seq)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                id,
                serde_json::to_string(&lesson.kind)
                    .map_err(|e| LedgerError::Corrupt(e.to_string()))?
                    .trim_matches('"'),
                lesson.trigger,
                lesson.mechanism,
                lesson.claim,
                i64::from(lesson.applied),
                i64::from(lesson.helped),
                // The handle's, never the argument's.
                self.bucket(),
                seq,
            ],
        )?;
        for row_id in cites {
            conn.execute(
                "INSERT OR IGNORE INTO lesson_evidence(lesson_id, row_id) VALUES(?1,?2)",
                params![id, row_id],
            )?;
        }
        Ok(id)
    }

    async fn lessons(&self, kind: Option<LessonKind>) -> Result<Vec<Lesson>> {
        let conn = self.guard()?;
        // This bucket plus global. An unscoped handle's bucket is global, so
        // the two halves coincide and it sees exactly what it wrote.
        let mut stmt = conn
            .prepare("SELECT * FROM lessons WHERE scope_key = ?1 OR scope_key = '' ORDER BY seq")?;
        let all = stmt
            .query_map([self.bucket()], |r| {
                let scope: String = r.get("scope_key")?;
                Ok(Lesson {
                    id: r.get("id")?,
                    kind: LessonKind::parse(&r.get::<_, String>("kind")?),
                    trigger: r.get("trigger")?,
                    mechanism: r.get("mechanism")?,
                    claim: r.get("claim")?,
                    applied: r.get::<_, i64>("applied")?.try_into().unwrap_or(0),
                    helped: r.get::<_, i64>("helped")?.try_into().unwrap_or(0),
                    scope_key: (!scope.is_empty()).then_some(scope),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(match kind {
            Some(want) => all.into_iter().filter(|l| l.kind == want).collect(),
            None => all,
        })
    }

    async fn evidence(&self, lesson_id: &str) -> Result<Vec<LedgerRow>> {
        let conn = self.guard()?;
        let mut stmt = conn.prepare(
            "SELECT r.* FROM ledger_rows r
             JOIN lesson_evidence e ON e.row_id = r.id
             WHERE e.lesson_id = ?1 AND r.scope_key = ?2 ORDER BY r.seq",
        )?;
        let found = stmt
            .query_map(params![lesson_id, self.bucket()], read_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(found)
    }

    async fn score_lesson(&self, lesson_id: &str, helped: bool) -> Result<()> {
        let conn = self.guard()?;
        conn.execute(
            // Constrained to what this handle can see — the id arrives from
            // model output, and naming another tenant's lesson must not move
            // its score.
            "UPDATE lessons SET applied = applied + 1, helped = helped + ?2
             WHERE id = ?1 AND (scope_key = ?3 OR scope_key = '')",
            params![lesson_id, i64::from(helped), self.bucket()],
        )?;
        Ok(())
    }

    async fn score_workflow(&self, workflow_id: &str, helped: bool) -> Result<()> {
        let conn = self.guard()?;
        // Upsert: the first run of a workflow is the common case and must not
        // need a separate registration step.
        conn.execute(
            "INSERT INTO workflow_scores(scope_key, workflow_id, applied, helped)
             VALUES(?1, ?2, 1, ?3)
             ON CONFLICT(scope_key, workflow_id) DO UPDATE SET
                applied = applied + 1,
                helped  = helped + ?3",
            params![self.bucket(), workflow_id, i64::from(helped)],
        )?;
        Ok(())
    }

    async fn workflow_score(&self, workflow_id: &str) -> Result<Score> {
        let conn = self.guard()?;
        let found = conn
            .query_row(
                "SELECT applied, helped FROM workflow_scores
                 WHERE scope_key = ?1 AND workflow_id = ?2",
                params![self.bucket(), workflow_id],
                |r| {
                    Ok(Score {
                        applied: r.get::<_, i64>(0)?.try_into().unwrap_or(0),
                        helped: r.get::<_, i64>(1)?.try_into().unwrap_or(0),
                    })
                },
            )
            .optional()?;
        Ok(found.unwrap_or_default())
    }

    async fn link_variant(&self, parent: &str, variant: &str) -> Result<()> {
        let conn = self.guard()?;
        conn.execute(
            "INSERT OR IGNORE INTO variants(scope_key, variant, parent) VALUES(?1,?2,?3)",
            params![self.bucket(), variant, parent],
        )?;
        Ok(())
    }

    async fn parent_of(&self, id: &str) -> Result<Option<String>> {
        let conn = self.guard()?;
        let found = conn
            .query_row(
                "SELECT parent FROM variants WHERE scope_key = ?1 AND variant = ?2",
                params![self.bucket(), id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(found)
    }

    async fn save_episode(&self, episode: &Episode) -> Result<()> {
        let conn = self.guard()?;
        conn.execute(
            "INSERT INTO episodes(id, scope_key, goal, status, attempt, stalled,
                                  started_at, updated_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8)
             ON CONFLICT(id) DO UPDATE SET
                goal = ?3, status = ?4, attempt = ?5, stalled = ?6, updated_at = ?8",
            params![
                episode.id,
                self.bucket(),
                serde_json::to_string(&episode.goal)
                    .map_err(|e| LedgerError::Corrupt(e.to_string()))?,
                serde_json::to_string(&episode.status)
                    .map_err(|e| LedgerError::Corrupt(e.to_string()))?,
                i64::from(episode.attempt),
                i64::from(episode.stalled),
                episode.started_at,
                episode.updated_at,
            ],
        )?;
        Ok(())
    }

    async fn episode(&self, id: &str) -> Result<Option<Episode>> {
        let conn = self.guard()?;
        let found = conn
            .query_row(
                "SELECT * FROM episodes WHERE id = ?1 AND scope_key = ?2",
                params![id, self.bucket()],
                read_episode,
            )
            .optional()?;
        found.transpose()
    }

    async fn save_steps(&self, row_id: &str, steps: &[crate::execute::StepRecord]) -> Result<()> {
        let conn = self.guard()?;
        // Replace, not overlay: `INSERT OR REPLACE` only touches the sequence
        // numbers present in `steps`, so a shorter re-save would leave the old
        // tail behind and `steps()` would stitch two attempts together.
        conn.execute(
            "DELETE FROM attempt_steps WHERE scope_key = ?1 AND row_id = ?2",
            params![self.bucket(), row_id],
        )?;
        for (seq, step) in steps.iter().enumerate() {
            conn.execute(
                "INSERT OR REPLACE INTO attempt_steps(scope_key, row_id, seq, node_id, status,
                                                      output, duration_ms, null_bindings,
                                                      transcript)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![
                    self.bucket(),
                    row_id,
                    i64::try_from(seq).unwrap_or(i64::MAX),
                    step.node_id,
                    serde_json::to_string(&step.status)
                        .map_err(|e| LedgerError::Corrupt(e.to_string()))?
                        .trim_matches('"'),
                    serde_json::to_string(&step.output)
                        .map_err(|e| LedgerError::Corrupt(e.to_string()))?,
                    i64::try_from(step.duration_ms).unwrap_or(i64::MAX),
                    serde_json::to_string(&step.null_bindings)
                        .map_err(|e| LedgerError::Corrupt(e.to_string()))?,
                    serde_json::to_string(&step.transcript)
                        .map_err(|e| LedgerError::Corrupt(e.to_string()))?,
                ],
            )?;
        }
        Ok(())
    }

    async fn steps(&self, row_id: &str) -> Result<Vec<crate::execute::StepRecord>> {
        let conn = self.guard()?;
        let mut stmt = conn.prepare(
            "SELECT node_id, status, output, duration_ms, null_bindings, transcript
             FROM attempt_steps
             WHERE scope_key = ?1 AND row_id = ?2 ORDER BY seq",
        )?;
        let found = stmt
            .query_map(params![self.bucket(), row_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        found
            .into_iter()
            .map(
                |(node_id, status, output, duration_ms, bindings, transcript)| {
                    Ok(crate::execute::StepRecord {
                        node_id,
                        status: if status == "error" {
                            crate::execute::StepOutcome::Error
                        } else {
                            crate::execute::StepOutcome::Success
                        },
                        output: serde_json::from_str(&output)
                            .map_err(|e| LedgerError::Corrupt(e.to_string()))?,
                        duration_ms: u64::try_from(duration_ms).unwrap_or(0),
                        null_bindings: serde_json::from_str(&bindings)
                            .map_err(|e| LedgerError::Corrupt(e.to_string()))?,
                        transcript: serde_json::from_str(&transcript)
                            .map_err(|e| LedgerError::Corrupt(e.to_string()))?,
                    })
                },
            )
            .collect()
    }

    async fn episodes(&self, running_only: bool, page: super::Page) -> Result<Vec<Episode>> {
        let conn = self.guard()?;
        let mut stmt = conn
            .prepare("SELECT * FROM episodes WHERE scope_key = ?1 ORDER BY updated_at DESC, id")?;
        let all = stmt
            .query_map([self.bucket()], read_episode)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let kept: Result<Vec<Episode>> = all
            .into_iter()
            .filter(|e| {
                !running_only || e.as_ref().is_ok_and(|e| e.status == EpisodeStatus::Running)
            })
            .collect();
        Ok(page.apply(kept?))
    }

    async fn children_of(&self, id: &str) -> Result<Vec<String>> {
        let conn = self.guard()?;
        let mut stmt = conn.prepare(
            "SELECT variant FROM variants WHERE scope_key = ?1 AND parent = ?2 ORDER BY variant",
        )?;
        let found = stmt
            .query_map(params![self.bucket(), id], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?;
        Ok(found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::conformance;

    #[tokio::test]
    async fn passes_the_conformance_suite() {
        let store = SqliteLedger::in_memory().expect("open in-memory ledger");
        conformance::run_all(&store).await;
    }

    #[tokio::test]
    async fn passes_the_tenant_isolation_suite() {
        let store = SqliteLedger::in_memory().expect("open in-memory ledger");
        let a = store.for_tenant("user-a");
        let b = store.for_tenant("user-b");
        conformance::run_tenants(&store, &a, &b).await;
    }

    #[tokio::test]
    async fn a_scoped_handle_shares_the_connection_rather_than_the_file() {
        // Two handles for the SAME tenant must see each other's writes — that
        // is what "shares" means. Probing it across scopes would now fail by
        // design, because rows carry the bucket that wrote them.
        let store = SqliteLedger::in_memory().expect("open in-memory ledger");
        let one = store.for_tenant("user-a");
        let two = store.for_tenant("user-a");
        one.append(&conformance::row("ep-shared", 1, "authored"))
            .await
            .expect("append");
        assert_eq!(two.rows("ep-shared").await.expect("rows").len(), 1);
        assert!(
            store.rows("ep-shared").await.expect("rows").is_empty(),
            "and the global bucket is its own, not a union"
        );
    }

    #[tokio::test]
    async fn opening_a_path_creates_the_directory_holding_it() {
        // A first run against `/var/lib/whatever/ledger.db` must not fail
        // because nobody made the folder.
        let root = std::env::temp_dir().join(format!("adaptive-mkdir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let path = root.join("deep").join("nested").join("ledger.db");

        let store = SqliteLedger::open(&path).expect("open");
        store
            .append(&conformance::row("ep-mkdir", 1, "authored"))
            .await
            .expect("append");
        assert!(path.exists(), "{}", path.display());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_environment_moves_the_ledger_without_a_rebuild() {
        let fallback = std::path::Path::new("/srv/app/ledger.db");
        assert_eq!(
            chosen_path(Some("/mnt/data/ledger.db"), fallback),
            std::path::PathBuf::from("/mnt/data/ledger.db")
        );
    }

    #[test]
    fn an_unset_environment_falls_back_to_the_path_in_the_code() {
        let fallback = std::path::Path::new("/srv/app/ledger.db");
        assert_eq!(chosen_path(None, fallback), fallback);
    }

    #[test]
    fn a_blank_variable_reads_as_unset_rather_than_as_an_empty_path() {
        // What a shell leaves behind when a value was meant to be interpolated
        // and was not. Opening "" fails in a way that names nothing useful.
        let fallback = std::path::Path::new("/srv/app/ledger.db");
        assert_eq!(chosen_path(Some(""), fallback), fallback);
        assert_eq!(chosen_path(Some("   "), fallback), fallback);
    }

    fn fake_env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |key: &str| owned.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }

    #[test]
    fn each_platform_uses_its_own_documented_directory() {
        let home = fake_env(&[("HOME", "/home/ada")]);
        assert_eq!(
            data_dir(Platform::Xdg, &home),
            Some("/home/ada/.local/share".into())
        );
        assert_eq!(
            data_dir(Platform::MacOs, &fake_env(&[("HOME", "/Users/ada")])),
            Some("/Users/ada/Library/Application Support".into())
        );
        assert_eq!(
            data_dir(
                Platform::Windows,
                &fake_env(&[("LOCALAPPDATA", "C:\\Users\\ada\\AppData\\Local")])
            ),
            Some("C:\\Users\\ada\\AppData\\Local".into())
        );
    }

    #[test]
    fn xdg_data_home_wins_over_the_spec_s_own_fallback() {
        let env = fake_env(&[("XDG_DATA_HOME", "/data"), ("HOME", "/home/ada")]);
        assert_eq!(data_dir(Platform::Xdg, &env), Some("/data".into()));
    }

    #[test]
    fn windows_uses_the_local_profile_not_the_roaming_one() {
        // A roaming profile syncs between machines, and a SQLite file copied
        // mid-write between two that both think they own it is a corrupted
        // database. Setting only APPDATA must therefore find nothing.
        let roaming = fake_env(&[("APPDATA", "C:\\Users\\ada\\AppData\\Roaming")]);
        assert_eq!(data_dir(Platform::Windows, &roaming), None);
    }

    #[test]
    fn the_conventional_path_is_namespaced_by_project_and_named_for_the_crate() {
        let env = fake_env(&[("HOME", "/home/ada")]);
        assert_eq!(
            default_path(Platform::Xdg, &env).expect("path"),
            std::path::PathBuf::from("/home/ada/.local/share/tinyflows/adaptive.db")
        );
    }

    #[test]
    fn the_variable_still_wins_over_the_convention() {
        let env = fake_env(&[(DB_PATH_VAR, "/mnt/data/ledger.db"), ("HOME", "/home/ada")]);
        assert_eq!(
            default_path(Platform::Xdg, &env).expect("path"),
            std::path::PathBuf::from("/mnt/data/ledger.db")
        );
    }

    #[test]
    fn nowhere_conventional_is_an_error_that_says_what_to_set() {
        // A daemon under a user with no home. Guessing would put a database
        // somewhere nobody looks, and losing it silently is the failure this
        // whole crate is written to avoid.
        let err = default_path(Platform::Xdg, &fake_env(&[])).expect_err("no home");
        assert!(err.to_string().contains(DB_PATH_VAR), "{err}");
    }

    #[test]
    fn a_configured_path_is_trimmed() {
        let fallback = std::path::Path::new("/srv/app/ledger.db");
        assert_eq!(
            chosen_path(Some("  /mnt/data/ledger.db\n"), fallback),
            std::path::PathBuf::from("/mnt/data/ledger.db")
        );
    }

    #[tokio::test]
    async fn a_reopened_ledger_still_has_its_rows() {
        // The whole point of the sqlite backend over the in-memory one.
        let dir = std::env::temp_dir().join(format!("adaptive-ledger-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("ledger.db");
        let _ = std::fs::remove_file(&path);

        {
            let store = SqliteLedger::open(&path).expect("open");
            store
                .append(&conformance::row("ep", 1, "authored"))
                .await
                .expect("append");
        }
        let reopened = SqliteLedger::open(&path).expect("reopen");
        assert_eq!(reopened.rows("ep").await.expect("rows").len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn insertion_order_survives_a_timestamp_tie() {
        // Two attempts finishing in the same second is common; ordering by `at`
        // would make the exclusion list arbitrary.
        let store = SqliteLedger::in_memory().expect("open");
        for sig in ["first", "second", "third"] {
            let mut r = conformance::row("tie", 1, sig);
            r.at = "2026-01-01T00:00:00Z".to_string();
            store.append(&r).await.expect("append");
        }
        assert_eq!(
            store.tried("tie").await.expect("tried"),
            vec!["first", "second", "third"]
        );
    }
}
