//! SQLite persistence for the flow catalog.
//!
//! Every entry point takes the catalog **directory** as its first argument and
//! returns `anyhow::Result<T>`; `flows.db` is created and migrated inside it on
//! first use. A host passes whichever directory it keeps workflow state in — the
//! crate knows nothing about how that path was chosen.
//!
//! Two tables:
//! - `flow_definitions` — one row per saved [`Flow`], with the graph stored as
//!   JSON text (`graph_json`).
//! - `flow_state` — a generic namespaced key/value table a host binds to
//!   `tinyflows::caps::StateStore`.
//!
//! There is deliberately **no** `flow_checkpoints` table here: engine
//! checkpoints live in their own `checkpoints.db`, written by
//! [`crate::checkpoint`].

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

mod definitions;
mod kv;
mod revisions;
mod run_steps;
mod runs;
mod suggestions;

pub use definitions::*;
pub use kv::*;
pub use revisions::*;
pub use run_steps::*;
pub use runs::*;
pub use suggestions::*;

/// Diagnostic marker recording which flows database files this process has
/// initialized (R-m8). It no longer *gates* the DDL — the on-disk
/// `PRAGMA user_version` does that (see [`ensure_schema_initialized`]) — it only
/// distinguishes an ordinary first-ever init (path absent ⇒ silent) from a
/// database this process already initialized whose schema has since drifted on
/// disk (path present, version stale ⇒ worth a warning).
///
/// `with_connection` deliberately keeps opening a fresh, lightweight
/// `rusqlite::Connection` per call — `Connection` is `!Sync`, so caching a
/// single shared one would need a process-wide mutex that serializes every
/// caller, including the concurrent-writer scenario [`upsert_flow_run_step`]'s
/// `BEGIN IMMEDIATE` fix (R-m1) depends on being able to run from independent
/// connections. What actually repeated needlessly on every open was the DDL
/// batch itself — including once per node per live run via
/// `upsert_flow_run_step`; the version gate now keeps it to one execution per
/// process per database file while every call still gets its own connection.
///
/// Keyed by path rather than a single flag: tests each open an independent
/// per-`TempDir` workspace within the same test binary, and a bare
/// `OnceLock<()>` would report the wrong marker for every database path after
/// the first opened in the process.
static INITIALIZED_SCHEMAS: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();

/// On-disk schema version stamped into `flows.db`'s `PRAGMA user_version` by
/// [`init_schema`] and checked by [`ensure_schema_initialized`]. **Bump this
/// whenever a table or `add_column_if_missing` migration is added to
/// [`init_schema`]** so a database replaced at runtime with an older/partial
/// schema is re-migrated rather than trusted.
///
/// Distinct from `tinyflows::model::CURRENT_SCHEMA_VERSION`, which versions the
/// graph JSON payload — this versions the SQLite file's own schema.
const FLOWS_DB_SCHEMA_VERSION: i64 = 1;

/// Ensures the schema on `conn` (a connection to `db_path`) is present and fully
/// migrated, running the DDL + migrations at most once per process per path.
///
/// **The on-disk `PRAGMA user_version` is the authority, and the fast path is
/// lock-free.** [`init_schema`] stamps [`FLOWS_DB_SCHEMA_VERSION`] only after a
/// full, successful migration, so a connection whose `user_version` already
/// matches is known-good and returns without touching any process-global state —
/// the common case (an already-initialized store) never acquires the
/// initialization mutex. This matters here specifically: `with_connection` (and
/// so this check) runs once per node per live run via `upsert_flow_run_step`,
/// not just at store open, so keeping the hot path lock-free is load-bearing.
/// Reading `user_version` is a single database-header read, far cheaper than the
/// ~11-statement DDL batch it replaces.
///
/// **Initialization is serialized and atomic per process.** Only a version
/// *mismatch* takes the [`INITIALIZED_SCHEMAS`] lock, and `user_version` is
/// re-read under it, so two first callers racing on the same fresh path run the
/// DDL exactly once — the loser observes the winner's stamp on the recheck and
/// returns. Independent database paths contend only during that rare init
/// window, never on the hot path.
///
/// **Trust, but verify.** Before this gating existed the DDL ran on every
/// `with_connection` call, so a database deleted or replaced at runtime — a
/// workspace reset, a manual deletion, a disk-recovery restore — self-healed on
/// the next call: `Connection::open` creates a fresh empty file and
/// `CREATE TABLE IF NOT EXISTS` repopulates it. The version gate restores that
/// for the *whole* schema, not just one table's presence: a stale/zero
/// `user_version` — a fresh file, or an older/partial schema swapped in under a
/// live process (`flow_definitions` present but missing a migrated column such
/// as `require_approval` / `graph_hash`, or one of the other tables) — falls
/// through to the idempotent [`init_schema`] and is re-migrated rather than
/// trusted and later failing with `no such table` / `no such column`. A
/// single-table `sqlite_master` probe could not catch column drift.
///
/// [`INITIALIZED_SCHEMAS`] no longer gates the DDL — the on-disk version does —
/// and is kept purely as a **diagnostic marker**: a path present in the set
/// whose on-disk version no longer matches was initialized by *this* process and
/// has since been deleted or replaced, which is worth a warning; a path absent
/// from the set is an ordinary first-ever init and stays silent.
fn ensure_schema_initialized(conn: &Connection, db_path: &Path) -> Result<()> {
    let is_current = || -> bool {
        conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap_or(0)
            == FLOWS_DB_SCHEMA_VERSION
    };

    // Lock-free fast path: an already-migrated database carries
    // FLOWS_DB_SCHEMA_VERSION in its header, so the common case never acquires
    // the process-global initialization mutex.
    if is_current() {
        return Ok(());
    }

    // Mismatch ⇒ (re-)initialization is required. Serialize it so two first
    // callers for the same path cannot both run the DDL, and re-read the version
    // under the lock — another thread may have migrated between the lock-free
    // read above and acquiring the guard.
    let initialized = INITIALIZED_SCHEMAS.get_or_init(|| Mutex::new(HashSet::new()));
    let mut guard = initialized
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if is_current() {
        return Ok(());
    }

    // Diagnostic only (see the doc comment): a cached path whose on-disk schema
    // no longer matches was deleted or replaced under a live process — a
    // first-ever init leaves the path absent and stays silent.
    if guard.contains(db_path) {
        tracing::warn!(
            target: "flows",
            db = %db_path.display(),
            "[flows] a database this process already initialized no longer matches the expected schema version (deleted or replaced at runtime?) — re-running schema init"
        );
    }

    init_schema(conn)?;
    guard.insert(db_path.to_path_buf());
    Ok(())
}

/// The actual schema DDL: 5 `CREATE TABLE IF NOT EXISTS` + 6 `CREATE INDEX IF
/// NOT EXISTS` + `PRAGMA journal_mode = WAL` (a persistent db-file setting,
/// not per-connection — safe, and now guaranteed, to run only once) plus the
/// `require_approval` / `graph_hash` post-hoc column migrations. Split out of
/// `with_connection` so [`ensure_schema_initialized`] can gate it (R-m8).
/// Stamps [`FLOWS_DB_SCHEMA_VERSION`] into `user_version` last, so the gate can
/// distinguish a fully-migrated database from an older/partial one on a
/// cache hit.
fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         CREATE TABLE IF NOT EXISTS flow_definitions (
            id          TEXT PRIMARY KEY,
            name        TEXT NOT NULL,
            graph_json  TEXT NOT NULL,
            enabled     INTEGER NOT NULL DEFAULT 1,
            created_at  TEXT NOT NULL,
            updated_at  TEXT NOT NULL,
            last_run_at TEXT,
            last_status TEXT
         );
         CREATE INDEX IF NOT EXISTS idx_flow_definitions_enabled ON flow_definitions(enabled);

         CREATE TABLE IF NOT EXISTS flow_state (
            namespace TEXT NOT NULL,
            key       TEXT NOT NULL,
            value     TEXT NOT NULL,
            PRIMARY KEY (namespace, key)
         );

         CREATE TABLE IF NOT EXISTS flow_runs (
            id                      TEXT PRIMARY KEY,
            flow_id                 TEXT NOT NULL,
            thread_id               TEXT NOT NULL,
            status                  TEXT NOT NULL,
            started_at              TEXT NOT NULL,
            finished_at             TEXT,
            steps_json              TEXT NOT NULL DEFAULT '[]',
            pending_approvals_json  TEXT NOT NULL DEFAULT '[]',
            error                   TEXT,
            graph_hash              TEXT,
            FOREIGN KEY (flow_id) REFERENCES flow_definitions(id) ON DELETE CASCADE
         );
         CREATE INDEX IF NOT EXISTS idx_flow_runs_flow_id ON flow_runs(flow_id);
         CREATE INDEX IF NOT EXISTS idx_flow_runs_started_at ON flow_runs(started_at);

         CREATE TABLE IF NOT EXISTS flow_suggestions (
            id                     TEXT PRIMARY KEY,
            title                  TEXT NOT NULL,
            one_liner              TEXT NOT NULL,
            rationale              TEXT NOT NULL,
            trigger_hint           TEXT,
            steps_json             TEXT NOT NULL DEFAULT '[]',
            connections_json       TEXT NOT NULL DEFAULT '[]',
            slugs_json             TEXT NOT NULL DEFAULT '[]',
            build_prompt           TEXT NOT NULL,
            confidence             REAL NOT NULL DEFAULT 0,
            status                 TEXT NOT NULL DEFAULT 'new',
            created_at             TEXT NOT NULL,
            source_run_id          TEXT
         );
         CREATE INDEX IF NOT EXISTS idx_flow_suggestions_status ON flow_suggestions(status);
         CREATE INDEX IF NOT EXISTS idx_flow_suggestions_created_at ON flow_suggestions(created_at);

         CREATE TABLE IF NOT EXISTS flow_revisions (
            id               TEXT PRIMARY KEY,
            flow_id          TEXT NOT NULL,
            graph_json       TEXT NOT NULL,
            name             TEXT NOT NULL,
            require_approval INTEGER NOT NULL DEFAULT 0,
            created_at       TEXT NOT NULL,
            FOREIGN KEY (flow_id) REFERENCES flow_definitions(id) ON DELETE CASCADE
         );
         CREATE INDEX IF NOT EXISTS idx_flow_revisions_flow_id ON flow_revisions(flow_id, created_at);",
    )
    .context("Failed to initialize flows schema")?;

    // `require_approval` (issue B2) — added post-hoc so a workspace created
    // before this column existed still opens cleanly. Mirrors
    // `cron::store`'s `add_column_if_missing` idiom.
    add_column_if_missing(
        conn,
        "flow_definitions",
        "require_approval",
        "INTEGER NOT NULL DEFAULT 0",
    )?;

    // T-M1 — added post-hoc so a workspace whose `flows.db` predates the
    // stale-approval graph pin still opens cleanly. A row written before this
    // migration reads back as `graph_hash IS NULL`, which `flows_resume`
    // treats as "unknown — allow, with a warning log" (see its doc), never as
    // a hard refusal, so upgrading mid-park cannot strand an in-flight
    // approval.
    add_column_if_missing(conn, "flow_runs", "graph_hash", "TEXT")?;

    // Stamp the schema version last, so [`ensure_schema_initialized`] only
    // trusts a cache hit whose on-disk schema is fully migrated. Bump
    // FLOWS_DB_SCHEMA_VERSION whenever a table or `add_column_if_missing`
    // migration is added above.
    conn.pragma_update(None, "user_version", FLOWS_DB_SCHEMA_VERSION)
        .context("Failed to stamp flows schema version")?;

    Ok(())
}

/// Opens (creating/migrating as needed — once per process per database file,
/// see [`ensure_schema_initialized`]) the flows SQLite database and runs `f`
/// against the connection.
fn with_connection<T>(dir: &Path, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    let db_path = dir.join("flows.db");
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create flows directory: {}", parent.display()))?;
    }

    let conn = Connection::open(&db_path)
        .with_context(|| format!("Failed to open flows DB: {}", db_path.display()))?;

    // Per-connection pragmas: NOT persisted in the database file, so these
    // must be reapplied on every open regardless of the schema-init cache
    // below. `busy_timeout` retries (rather than immediately erroring
    // `SQLITE_BUSY`) when a concurrent writer holds the lock — including this
    // store's own `BEGIN IMMEDIATE` step upsert (R-m1); `foreign_keys` is
    // required on every connection for the `ON DELETE CASCADE` FKs to be
    // enforced.
    conn.execute_batch("PRAGMA busy_timeout = 5000; PRAGMA foreign_keys = ON;")
        .context("Failed to set flows DB connection pragmas")?;

    ensure_schema_initialized(&conn, &db_path)?;

    tracing::debug!(db = %db_path.display(), "[flows] store opened");

    f(&conn)
}

/// Adds `name` to `table` if it isn't already present, tolerating the race
/// where a concurrent process adds the same column between the `PRAGMA`
/// check and the `ALTER TABLE`. Mirrors `cron::store::add_column_if_missing`
/// (kept per-domain rather than shared — each store owns its own connection
/// helper and this is a handful of lines).
fn add_column_if_missing(conn: &Connection, table: &str, name: &str, sql_type: &str) -> Result<()> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let col_name: String = row.get(1)?;
        if col_name == name {
            return Ok(());
        }
    }
    drop(rows);
    drop(stmt);

    match conn.execute(
        &format!("ALTER TABLE {table} ADD COLUMN {name} {sql_type}"),
        [],
    ) {
        Ok(_) => Ok(()),
        Err(rusqlite::Error::SqliteFailure(err, Some(ref msg)))
            if msg.contains("duplicate column name") =>
        {
            tracing::debug!(
                "[flows] column {table}.{name} already exists (concurrent migration): {err}"
            );
            Ok(())
        }
        Err(e) => Err(e).with_context(|| format!("Failed to add {table}.{name}")),
    }
}

fn sql_conversion_error<E: std::error::Error + Send + Sync + 'static>(err: E) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(err))
}

/// Runs `f` inside a `BEGIN IMMEDIATE` / `COMMIT` transaction on `conn`,
/// rolling back on error. `BEGIN IMMEDIATE` (rather than the default deferred
/// `BEGIN`) acquires SQLite's write lock immediately instead of only at the
/// first write statement, which is what closes the read-then-write race
/// [`upsert_flow_run_step`] needs closed (R-m1). Issued as raw SQL via
/// `execute_batch` rather than `rusqlite::Connection::transaction` (which
/// needs `&mut Connection`) so this can compose with `with_connection`'s
/// `&Connection` closure signature used by every other store function.
fn with_immediate_transaction<T>(
    conn: &Connection,
    f: impl FnOnce(&Connection) -> Result<T>,
) -> Result<T> {
    conn.execute_batch("BEGIN IMMEDIATE")
        .context("Failed to begin immediate transaction")?;
    match f(conn) {
        Ok(value) => {
            conn.execute_batch("COMMIT")
                .context("Failed to commit transaction")?;
            Ok(value)
        }
        Err(e) => {
            if let Err(rollback_err) = conn.execute_batch("ROLLBACK") {
                tracing::warn!(target: "flows", error = %rollback_err, "[flows] failed to roll back transaction after error");
            }
            Err(e)
        }
    }
}

#[cfg(test)]
pub(crate) mod test_support;

#[cfg(test)]
#[path = "schema_tests.rs"]
mod tests;
