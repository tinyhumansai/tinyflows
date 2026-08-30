//! SQLite-backed [`Checkpointer`] for flow runs — the host's own port of the
//! backend tinyflows dropped when it vendored its state-graph runtime in-crate
//! (tinyflows PR #43).
//!
//! Flow runs are durable and cross-process: `flows_run` resumes an interrupted
//! run from `<workspace_dir>/flows/checkpoints.db`. That database predates the
//! engine change, so switching to tinyflows' `FileCheckpointer` would have
//! stranded every in-flight run rather than merely changing a backend. The
//! trait tinyflows vendored is method-for-method the one `tinyagents` defines,
//! so this is that crate's `graph::checkpoint::sqlite` retargeted at
//! `tinyflows::graph` — same SQL, same schema, same on-disk format, so an
//! existing `checkpoints.db` is read and written exactly as before.
//!
//! Keep it that way: this is a port, not a rewrite. A behavioural change here
//! is a divergence from the runtime that reads the rows.
//!
//! Split by responsibility: this file owns the type, schema and shared
//! helpers; [`reads`] and [`writes`] hold the [`Checkpointer`] trait method
//! bodies as free functions (a single trait impl cannot itself span files, so
//! each trait method here is a thin delegator).

mod reads;
mod writes;

use std::marker::PhantomData;
use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rusqlite::Connection;
use serde::Serialize;
use serde::de::DeserializeOwned;

use tinyflows::graph::error::{GraphError, Result};
use tinyflows::graph::ids::{CheckpointId, NodeId};
use tinyflows::graph::{
    Checkpoint, CheckpointConfig, CheckpointMetadata, CheckpointSource, CheckpointTuple,
    Checkpointer, PendingWrite,
};

/// A [`Checkpointer`] that persists checkpoints in a SQLite database.
///
/// Cheap to clone; clones share the same underlying connection (and therefore
/// the same data, including for in-memory databases). Generic over `State`; the
/// [`Checkpointer`] impl requires `State: Serialize + DeserializeOwned`.
pub struct SqliteCheckpointer<State> {
    conn: Arc<Mutex<Connection>>,
    _marker: PhantomData<fn() -> State>,
}

impl<State> Clone for SqliteCheckpointer<State> {
    fn clone(&self) -> Self {
        Self {
            conn: self.conn.clone(),
            _marker: PhantomData,
        }
    }
}

fn sqlite_err(context: &str, err: impl std::fmt::Display) -> GraphError {
    GraphError::Checkpoint(format!("sqlite checkpointer: {context}: {err}"))
}

impl<State> SqliteCheckpointer<State> {
    /// Opens (creating if needed) a SQLite-backed checkpointer at `path`.
    ///
    /// Pass `":memory:"` for an ephemeral in-memory database (see
    /// [`SqliteCheckpointer::in_memory`]).
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path.as_ref()).map_err(|e| sqlite_err("open database", e))?;
        Self::from_connection(conn)
    }

    /// Opens an ephemeral in-memory checkpointer (`":memory:"`).
    ///
    /// The database lives only as long as this handle and its clones, which share
    /// the single underlying connection.
    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().map_err(|e| sqlite_err("open in-memory", e))?;
        Self::from_connection(conn)
    }

    /// Wraps a caller-owned open [`Connection`], ensuring the checkpoint schema
    /// exists.
    ///
    /// Use this to share a connection from your own pool or an existing
    /// application database instead of letting the checkpointer own its handle.
    /// The schema is idempotent (`CREATE TABLE IF NOT EXISTS`), so it is safe to
    /// call on a database that already has the tables.
    ///
    /// If your application depends on a *different* `rusqlite`/`libsqlite3-sys`
    /// version (a native-link conflict that prevents passing a `Connection`
    /// across the boundary), apply [`SqliteCheckpointer::schema_sql`] to your own
    /// connection instead and drive the tables directly.
    pub fn from_connection(conn: Connection) -> Result<Self> {
        conn.execute_batch(SCHEMA)
            .map_err(|e| sqlite_err("create schema", e))?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            _marker: PhantomData,
        })
    }

    /// Returns the checkpoint table + index DDL as a reusable, dependency-free
    /// SQL string.
    ///
    /// This is the schema-helper escape hatch for applications that own their
    /// own SQLite connection (possibly at an incompatible native-link version):
    /// execute this DDL on your connection to create the tables the checkpoint
    /// projection expects, without linking this crate's `rusqlite`.
    pub fn schema_sql() -> &'static str {
        SCHEMA
    }
}

/// Locks `conn`. Every [`Checkpointer`] method below moves its work onto
/// [`tokio::task::spawn_blocking`], where only the cloned `Arc` (not `self`)
/// is available, so this is a free function rather than a method.
fn lock_conn(conn: &Arc<Mutex<Connection>>) -> Result<std::sync::MutexGuard<'_, Connection>> {
    conn.lock().map_err(|_| {
        GraphError::Checkpoint("sqlite checkpointer: connection lock poisoned".to_string())
    })
}

/// Table + indexes. `seq` preserves insertion order; the indexes serve thread
/// listing, `(thread_id, checkpoint_id)` parent-chain lookups, and — since the
/// namespace-scoped overrides landed — `(thread_id, namespace, …)` scoped
/// lookups.
///
/// # `namespace` is a first-class, indexed column
///
/// It holds the canonical JSON encoding of the namespace vector, which
/// `serde_json` emits deterministically, so equality on the column is exactly
/// equality on the namespace. It was already stored this way; what was missing
/// were the indexes, and therefore the ability to *push the scope down into
/// SQL* at all. Without them `get_scoped`, `get_tuple` and `state_history` all
/// fell back to the trait defaults, which scan the whole thread once per
/// lineage hop — O(H²) per namespaced read on the one backend that had no
/// business being in that class. Both are `CREATE INDEX IF NOT EXISTS`, so an
/// existing database picks them up on the next open with no migration step.
///
/// `checkpoint_writes` is the partial-failure ledger. Its primary key
/// `(thread_id, namespace, checkpoint_id, task_id, idx)` mirrors LangGraph's
/// writes table and is what makes `put_writes` idempotent in SQL rather than in
/// application code.
const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS checkpoints (
    seq                  INTEGER PRIMARY KEY AUTOINCREMENT,
    thread_id            TEXT    NOT NULL,
    checkpoint_id        TEXT    NOT NULL,
    parent_checkpoint_id TEXT,
    run_id               TEXT,
    namespace            TEXT    NOT NULL,
    next_nodes           TEXT    NOT NULL,
    source               TEXT    NOT NULL,
    step                 INTEGER NOT NULL,
    has_interrupts       INTEGER NOT NULL,
    record               TEXT    NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_checkpoints_thread ON checkpoints (thread_id, seq);
CREATE INDEX IF NOT EXISTS idx_checkpoints_lookup ON checkpoints (thread_id, checkpoint_id);
CREATE INDEX IF NOT EXISTS idx_checkpoints_scoped ON checkpoints (thread_id, namespace, seq);
CREATE INDEX IF NOT EXISTS idx_checkpoints_scoped_lookup
    ON checkpoints (thread_id, namespace, checkpoint_id, seq);

CREATE TABLE IF NOT EXISTS checkpoint_writes (
    thread_id     TEXT    NOT NULL,
    namespace     TEXT    NOT NULL,
    checkpoint_id TEXT    NOT NULL,
    task_id       TEXT    NOT NULL,
    idx           INTEGER NOT NULL,
    node          TEXT    NOT NULL,
    channel       TEXT    NOT NULL,
    payload       TEXT    NOT NULL,
    PRIMARY KEY (thread_id, namespace, checkpoint_id, task_id, idx)
);
CREATE INDEX IF NOT EXISTS idx_checkpoint_writes_thread
    ON checkpoint_writes (thread_id, checkpoint_id);
";

/// tinyflows keeps its own copy `pub(crate)`, so the port carries one. Same
/// message: a caller comparing the two backends' errors sees no difference.
fn require_checkpoint_id(config: &CheckpointConfig) -> Result<String> {
    config.checkpoint_id.clone().ok_or_else(|| {
        GraphError::Checkpoint(format!(
            "put_writes requires an explicit checkpoint_id (thread `{}`)",
            config.thread_id
        ))
    })
}

/// The projected listing columns read from one `checkpoints` row.
struct MetaRow {
    thread_id: String,
    checkpoint_id: String,
    run_id: Option<String>,
    parent_checkpoint_id: Option<String>,
    namespace_json: String,
    next_nodes_json: String,
    source: String,
    step: i64,
    has_interrupts: i64,
}

/// Reconstructs a [`CheckpointMetadata`] from the projected listing columns,
/// without touching the full serialized record.
fn row_metadata(row: MetaRow) -> Result<CheckpointMetadata> {
    let namespace: Vec<String> =
        serde_json::from_str(&row.namespace_json).map_err(|e| sqlite_err("decode namespace", e))?;
    let next_nodes: Vec<NodeId> = serde_json::from_str(&row.next_nodes_json)
        .map_err(|e| sqlite_err("decode next_nodes", e))?;
    Ok(CheckpointMetadata {
        thread_id: row.thread_id,
        checkpoint_id: row.checkpoint_id,
        run_id: row.run_id,
        parent_checkpoint_id: row.parent_checkpoint_id,
        namespace,
        next_nodes,
        has_interrupts: row.has_interrupts != 0,
        source: CheckpointSource::parse(&row.source).unwrap_or(CheckpointSource::Loop),
        step: row.step as usize,
    })
}

#[async_trait]
impl<State> Checkpointer<State> for SqliteCheckpointer<State>
where
    State: Serialize + DeserializeOwned + Send + Sync + 'static,
{
    async fn put(&self, checkpoint: Checkpoint<State>) -> Result<CheckpointId> {
        writes::put(&self.conn, checkpoint).await
    }

    async fn get(
        &self,
        thread_id: &str,
        checkpoint_id: Option<&str>,
    ) -> Result<Option<Checkpoint<State>>> {
        reads::get(&self.conn, thread_id, checkpoint_id).await
    }

    async fn get_scoped(
        &self,
        thread_id: &str,
        checkpoint_id: Option<&str>,
        namespace: &[String],
    ) -> Result<Option<Checkpoint<State>>> {
        reads::get_scoped(&self.conn, thread_id, checkpoint_id, namespace).await
    }

    async fn state_history(
        &self,
        thread_id: &str,
        namespace: &[String],
        limit: Option<usize>,
    ) -> Result<Vec<CheckpointTuple<State>>> {
        reads::state_history(&self.conn, thread_id, namespace, limit).await
    }

    async fn list(&self, thread_id: &str) -> Result<Vec<CheckpointMetadata>> {
        reads::list(&self.conn, thread_id).await
    }

    async fn get_thread(&self, thread_id: &str) -> Result<Vec<Checkpoint<State>>> {
        reads::get_thread(&self.conn, thread_id).await
    }

    async fn list_threads(&self) -> Result<Vec<String>> {
        reads::list_threads(&self.conn).await
    }

    async fn delete_thread(&self, thread_id: &str) -> Result<()> {
        writes::delete_thread(&self.conn, thread_id).await
    }

    async fn delete_checkpoints(&self, thread_id: &str, ids: &[String]) -> Result<usize> {
        writes::delete_checkpoints(&self.conn, thread_id, ids).await
    }

    async fn put_writes(
        &self,
        config: &CheckpointConfig,
        writes_batch: &[PendingWrite],
    ) -> Result<()> {
        writes::put_writes(&self.conn, config, writes_batch).await
    }

    async fn get_writes(&self, config: &CheckpointConfig) -> Result<Vec<PendingWrite>> {
        let Some(checkpoint_id) = self.resolve_write_target(config).await? else {
            return Ok(Vec::new());
        };
        writes::get_writes(&self.conn, config, &checkpoint_id).await
    }
}
