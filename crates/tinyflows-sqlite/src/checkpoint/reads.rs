//! Read-side [`super::Checkpointer`] method bodies: `get`, `get_scoped`,
//! `state_history`, `list`, `get_thread`, `list_threads`. Each is a free
//! function the trait impl in [`super`] delegates to — a single trait impl
//! cannot itself span files, so this is where the split lives.

use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde::de::DeserializeOwned;

use tinyflows::graph::error::Result;
use tinyflows::graph::{Checkpoint, CheckpointConfig, CheckpointMetadata, CheckpointTuple};

use super::writes::read_writes_by_checkpoint;
use super::{MetaRow, lock_conn, row_metadata, sqlite_err};

pub(super) async fn get<State>(
    conn: &Arc<Mutex<Connection>>,
    thread_id: &str,
    checkpoint_id: Option<&str>,
) -> Result<Option<Checkpoint<State>>>
where
    State: Serialize + DeserializeOwned + Send + Sync + 'static,
{
    // Synchronous rusqlite I/O behind a `std::sync::Mutex` — run it on the
    // blocking pool so a file-backed database, or another checkpoint call
    // already holding the mutex, never stalls a tokio worker.
    let conn = conn.clone();
    let thread_id = thread_id.to_string();
    let checkpoint_id = checkpoint_id.map(str::to_string);
    tokio::task::spawn_blocking(move || -> Result<Option<Checkpoint<State>>> {
        let conn = lock_conn(&conn)?;
        // Latest matching row (highest seq) for either the whole thread or a
        // specific id, mirroring the append-only history of the other backends.
        let record: Option<String> = match checkpoint_id.as_deref() {
            Some(id) => conn
                .query_row(
                    "SELECT record FROM checkpoints
                     WHERE thread_id = ?1 AND checkpoint_id = ?2
                     ORDER BY seq DESC LIMIT 1",
                    params![thread_id, id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|e| sqlite_err("query checkpoint", e))?,
            None => conn
                .query_row(
                    "SELECT record FROM checkpoints
                     WHERE thread_id = ?1
                     ORDER BY seq DESC LIMIT 1",
                    params![thread_id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|e| sqlite_err("query latest checkpoint", e))?,
        };
        match record {
            Some(json) => Ok(Some(
                serde_json::from_str(&json).map_err(|e| sqlite_err("decode record", e))?,
            )),
            None => Ok(None),
        }
    })
    .await
    .map_err(|e| sqlite_err("join blocking get task", e))?
}

pub(super) async fn get_scoped<State>(
    conn: &Arc<Mutex<Connection>>,
    thread_id: &str,
    checkpoint_id: Option<&str>,
    namespace: &[String],
) -> Result<Option<Checkpoint<State>>>
where
    State: Serialize + DeserializeOwned + Send + Sync + 'static,
{
    // Pushed down to one indexed query. The trait default lists the whole
    // thread and then re-`get`s the winner, which costs a full thread scan
    // per call — and `state_history` calls it once per lineage hop.
    let namespace_json =
        serde_json::to_string(namespace).map_err(|e| sqlite_err("encode namespace", e))?;
    let conn = conn.clone();
    let thread_id = thread_id.to_string();
    let checkpoint_id = checkpoint_id.map(str::to_string);
    tokio::task::spawn_blocking(move || -> Result<Option<Checkpoint<State>>> {
        let conn = lock_conn(&conn)?;
        let record: Option<String> = match checkpoint_id.as_deref() {
            Some(id) => conn
                .query_row(
                    "SELECT record FROM checkpoints
                     WHERE thread_id = ?1 AND namespace = ?2 AND checkpoint_id = ?3
                     ORDER BY seq DESC LIMIT 1",
                    params![thread_id, namespace_json, id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|e| sqlite_err("query scoped checkpoint", e))?,
            None => conn
                .query_row(
                    "SELECT record FROM checkpoints
                     WHERE thread_id = ?1 AND namespace = ?2
                     ORDER BY seq DESC LIMIT 1",
                    params![thread_id, namespace_json],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|e| sqlite_err("query latest scoped checkpoint", e))?,
        };
        match record {
            Some(json) => Ok(Some(
                serde_json::from_str(&json).map_err(|e| sqlite_err("decode record", e))?,
            )),
            None => Ok(None),
        }
    })
    .await
    .map_err(|e| sqlite_err("join blocking get_scoped task", e))?
}

pub(super) async fn state_history<State>(
    conn: &Arc<Mutex<Connection>>,
    thread_id: &str,
    namespace: &[String],
    limit: Option<usize>,
) -> Result<Vec<CheckpointTuple<State>>>
where
    State: Serialize + DeserializeOwned + Send + Sync + 'static,
{
    // One indexed range read of the namespace's rows, then the lineage walk
    // in memory — instead of the default's `get_tuple` (and therefore
    // `get_scoped`) per hop.
    let namespace_json =
        serde_json::to_string(namespace).map_err(|e| sqlite_err("encode namespace", e))?;
    let conn = conn.clone();
    let thread_id = thread_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<Vec<CheckpointTuple<State>>> {
        let (records, writes) = {
            let conn = lock_conn(&conn)?;
            let mut stmt = conn
                .prepare(
                    "SELECT record FROM checkpoints
                     WHERE thread_id = ?1 AND namespace = ?2 ORDER BY seq ASC",
                )
                .map_err(|e| sqlite_err("prepare state_history", e))?;
            let rows = stmt
                .query_map(params![thread_id, namespace_json], |row| {
                    row.get::<_, String>(0)
                })
                .map_err(|e| sqlite_err("query state_history", e))?;
            let mut records: Vec<Checkpoint<State>> = Vec::new();
            for row in rows {
                let json = row.map_err(|e| sqlite_err("read record row", e))?;
                records
                    .push(serde_json::from_str(&json).map_err(|e| sqlite_err("decode record", e))?);
            }
            let writes = read_writes_by_checkpoint(&conn, &thread_id, &namespace_json)?;
            (records, writes)
        };
        if records.is_empty() {
            return Ok(Vec::new());
        }

        // Last write wins for a re-used id, matching `get`.
        let mut by_id: std::collections::HashMap<String, Checkpoint<State>> =
            std::collections::HashMap::with_capacity(records.len());
        let mut cursor: Option<String> = None;
        for record in records {
            cursor = Some(record.checkpoint_id.clone());
            by_id.insert(record.checkpoint_id.clone(), record);
        }

        let mut out = Vec::new();
        while let Some(id) = cursor {
            // Written as a nested `if` rather than the source's let-chain:
            // this crate is edition 2021, where let-chains do not parse.
            if let Some(limit) = limit {
                if out.len() >= limit {
                    break;
                }
            }
            // `remove` doubles as the cycle guard: each id is visited once.
            let Some(checkpoint) = by_id.remove(&id) else {
                break;
            };
            cursor = checkpoint.parent_checkpoint_id.clone();
            let config = CheckpointConfig {
                thread_id: checkpoint.thread_id.clone(),
                checkpoint_id: Some(checkpoint.checkpoint_id.clone()),
                namespace: checkpoint.namespace.clone(),
            };
            let parent_config =
                checkpoint
                    .parent_checkpoint_id
                    .as_ref()
                    .map(|parent| CheckpointConfig {
                        thread_id: checkpoint.thread_id.clone(),
                        checkpoint_id: Some(parent.clone()),
                        namespace: checkpoint.namespace.clone(),
                    });
            let pending_writes = writes
                .get(&checkpoint.checkpoint_id)
                .cloned()
                .unwrap_or_else(|| checkpoint.pending_writes.clone());
            out.push(CheckpointTuple {
                config,
                checkpoint,
                parent_config,
                pending_writes,
            });
        }
        Ok(out)
    })
    .await
    .map_err(|e| sqlite_err("join blocking state_history task", e))?
}

pub(super) async fn list(
    conn: &Arc<Mutex<Connection>>,
    thread_id: &str,
) -> Result<Vec<CheckpointMetadata>> {
    let conn = conn.clone();
    let thread_id = thread_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<Vec<CheckpointMetadata>> {
        let conn = lock_conn(&conn)?;
        let mut stmt = conn
            .prepare(
                "SELECT thread_id, checkpoint_id, run_id, parent_checkpoint_id,
                        namespace, next_nodes, source, step, has_interrupts
                 FROM checkpoints WHERE thread_id = ?1 ORDER BY seq ASC",
            )
            .map_err(|e| sqlite_err("prepare list", e))?;
        let rows = stmt
            .query_map(params![thread_id], |row| {
                Ok(MetaRow {
                    thread_id: row.get(0)?,
                    checkpoint_id: row.get(1)?,
                    run_id: row.get(2)?,
                    parent_checkpoint_id: row.get(3)?,
                    namespace_json: row.get(4)?,
                    next_nodes_json: row.get(5)?,
                    source: row.get(6)?,
                    step: row.get(7)?,
                    has_interrupts: row.get(8)?,
                })
            })
            .map_err(|e| sqlite_err("query list", e))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row_metadata(
                row.map_err(|e| sqlite_err("read list row", e))?,
            )?);
        }
        Ok(out)
    })
    .await
    .map_err(|e| sqlite_err("join blocking list task", e))?
}

pub(super) async fn get_thread<State>(
    conn: &Arc<Mutex<Connection>>,
    thread_id: &str,
) -> Result<Vec<Checkpoint<State>>>
where
    State: Serialize + DeserializeOwned + Send + Sync + 'static,
{
    // Single-pass bulk read: one indexed range query over the thread's
    // rows in insertion order, instead of the default's one point query
    // per listed id.
    let conn = conn.clone();
    let thread_id = thread_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<Vec<Checkpoint<State>>> {
        let conn = lock_conn(&conn)?;
        let mut stmt = conn
            .prepare("SELECT record FROM checkpoints WHERE thread_id = ?1 ORDER BY seq ASC")
            .map_err(|e| sqlite_err("prepare get_thread", e))?;
        let rows = stmt
            .query_map(params![thread_id], |row| row.get::<_, String>(0))
            .map_err(|e| sqlite_err("query get_thread", e))?;
        let mut out = Vec::new();
        for row in rows {
            let json = row.map_err(|e| sqlite_err("read record row", e))?;
            out.push(serde_json::from_str(&json).map_err(|e| sqlite_err("decode record", e))?);
        }
        Ok(out)
    })
    .await
    .map_err(|e| sqlite_err("join blocking get_thread task", e))?
}

pub(super) async fn list_threads(conn: &Arc<Mutex<Connection>>) -> Result<Vec<String>> {
    let conn = conn.clone();
    tokio::task::spawn_blocking(move || -> Result<Vec<String>> {
        let conn = lock_conn(&conn)?;
        let mut stmt = conn
            .prepare("SELECT DISTINCT thread_id FROM checkpoints")
            .map_err(|e| sqlite_err("prepare list_threads", e))?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| sqlite_err("query list_threads", e))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| sqlite_err("read thread row", e))?);
        }
        Ok(out)
    })
    .await
    .map_err(|e| sqlite_err("join blocking list_threads task", e))?
}
