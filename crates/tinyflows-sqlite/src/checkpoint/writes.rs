//! Write-side [`super::Checkpointer`] method bodies: `put`, `put_writes`,
//! `get_writes`, `delete_thread`, `delete_checkpoints`, plus the shared
//! write-decoding helpers `state_history` (in [`super::reads`]) also uses.

use std::sync::{Arc, Mutex};

use rusqlite::{Connection, params};
use serde::Serialize;
use serde::de::DeserializeOwned;

use tinyflows::graph::checkpoint::merge_writes;
use tinyflows::graph::error::Result;
use tinyflows::graph::ids::{CheckpointId, NodeId};
use tinyflows::graph::{Checkpoint, CheckpointConfig, PendingWrite};

use super::{lock_conn, require_checkpoint_id, sqlite_err};

pub(super) async fn put<State>(
    conn: &Arc<Mutex<Connection>>,
    checkpoint: Checkpoint<State>,
) -> Result<CheckpointId>
where
    State: Serialize + DeserializeOwned + Send + Sync + 'static,
{
    let id = CheckpointId::new(checkpoint.checkpoint_id.clone());
    // Serialize + the synchronous rusqlite insert (which also blocks on the
    // connection mutex) is blocking work; run it on the blocking pool so it
    // never stalls a tokio worker on the step-critical path.
    let conn = conn.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let meta = checkpoint.to_metadata();
        let namespace = serde_json::to_string(&checkpoint.namespace)
            .map_err(|e| sqlite_err("encode namespace", e))?;
        let next_nodes = serde_json::to_string(&checkpoint.next_nodes)
            .map_err(|e| sqlite_err("encode next_nodes", e))?;
        let record =
            serde_json::to_string(&checkpoint).map_err(|e| sqlite_err("encode record", e))?;

        let conn = lock_conn(&conn)?;
        conn.execute(
            "INSERT INTO checkpoints (
                thread_id, checkpoint_id, parent_checkpoint_id, run_id,
                namespace, next_nodes, source, step, has_interrupts, record
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                checkpoint.thread_id,
                checkpoint.checkpoint_id,
                checkpoint.parent_checkpoint_id,
                checkpoint.run_id,
                namespace,
                next_nodes,
                meta.source.as_str(),
                meta.step as i64,
                i64::from(meta.has_interrupts),
                record,
            ],
        )
        .map_err(|e| sqlite_err("insert checkpoint", e))?;
        Ok(())
    })
    .await
    .map_err(|e| sqlite_err("join blocking put task", e))??;
    Ok(id)
}

pub(super) async fn delete_thread(conn: &Arc<Mutex<Connection>>, thread_id: &str) -> Result<()> {
    let conn = conn.clone();
    let thread_id = thread_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut conn = lock_conn(&conn)?;
        let tx = conn
            .transaction()
            .map_err(|e| sqlite_err("begin delete_thread", e))?;
        tx.execute(
            "DELETE FROM checkpoints WHERE thread_id = ?1",
            params![thread_id],
        )
        .map_err(|e| sqlite_err("delete thread", e))?;
        // Writes go with the thread — and across *every* namespace, not just
        // the root one, or an embedded subgraph's ledger outlives its thread.
        tx.execute(
            "DELETE FROM checkpoint_writes WHERE thread_id = ?1",
            params![thread_id],
        )
        .map_err(|e| sqlite_err("delete thread writes", e))?;
        tx.commit()
            .map_err(|e| sqlite_err("commit delete_thread", e))?;
        Ok(())
    })
    .await
    .map_err(|e| sqlite_err("join blocking delete_thread task", e))?
}

pub(super) async fn delete_checkpoints(
    conn: &Arc<Mutex<Connection>>,
    thread_id: &str,
    ids: &[String],
) -> Result<usize> {
    if ids.is_empty() {
        return Ok(0);
    }
    let conn = conn.clone();
    let thread_id = thread_id.to_string();
    let ids = ids.to_vec();
    tokio::task::spawn_blocking(move || -> Result<usize> {
        let mut conn = lock_conn(&conn)?;
        let tx = conn
            .transaction()
            .map_err(|e| sqlite_err("begin transaction", e))?;
        let mut removed = 0usize;
        for id in &ids {
            removed += tx
                .execute(
                    "DELETE FROM checkpoints WHERE thread_id = ?1 AND checkpoint_id = ?2",
                    params![thread_id, id],
                )
                .map_err(|e| sqlite_err("delete checkpoint", e))?;
            tx.execute(
                "DELETE FROM checkpoint_writes WHERE thread_id = ?1 AND checkpoint_id = ?2",
                params![thread_id, id],
            )
            .map_err(|e| sqlite_err("delete checkpoint writes", e))?;
        }
        tx.commit().map_err(|e| sqlite_err("commit delete", e))?;
        Ok(removed)
    })
    .await
    .map_err(|e| sqlite_err("join blocking delete_checkpoints task", e))?
}

pub(super) async fn put_writes(
    conn: &Arc<Mutex<Connection>>,
    config: &CheckpointConfig,
    writes: &[PendingWrite],
) -> Result<()> {
    let checkpoint_id = require_checkpoint_id(config)?;
    if writes.is_empty() {
        return Ok(());
    }
    let namespace_json =
        serde_json::to_string(&config.namespace).map_err(|e| sqlite_err("encode namespace", e))?;
    let conn = conn.clone();
    let thread_id = config.thread_id.clone();
    let writes = writes.to_vec();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut conn = lock_conn(&conn)?;
        let tx = conn
            .transaction()
            .map_err(|e| sqlite_err("begin put_writes", e))?;
        let mut stored = 0usize;
        for write in &writes {
            // The replace-vs-ignore rule pushed into SQL: a control-plane write
            // (`idx < 0`) legitimately changes on a retry and upserts, while a
            // data write is append-once so a retried `put_writes` is a no-op.
            // Doing it with two conflict clauses rather than a read-then-write
            // keeps it correct under concurrent writers.
            let sql = if write.is_control_plane() {
                "INSERT INTO checkpoint_writes
                    (thread_id, namespace, checkpoint_id, task_id, idx, node, channel, payload)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(thread_id, namespace, checkpoint_id, task_id, idx) DO UPDATE SET
                    node = excluded.node,
                    channel = excluded.channel,
                    payload = excluded.payload"
            } else {
                "INSERT INTO checkpoint_writes
                    (thread_id, namespace, checkpoint_id, task_id, idx, node, channel, payload)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(thread_id, namespace, checkpoint_id, task_id, idx) DO NOTHING"
            };
            let payload = serde_json::to_string(&write.payload)
                .map_err(|e| sqlite_err("encode write payload", e))?;
            stored += tx
                .execute(
                    sql,
                    params![
                        thread_id,
                        namespace_json,
                        checkpoint_id,
                        write.task_id,
                        write.idx,
                        write.node.as_str(),
                        write.channel,
                        payload,
                    ],
                )
                .map_err(|e| sqlite_err("insert checkpoint write", e))?;
        }
        tx.commit()
            .map_err(|e| sqlite_err("commit put_writes", e))?;
        tracing::debug!(
            "[checkpoint:sqlite] put_writes thread={} checkpoint={checkpoint_id} offered={} stored={stored}",
            thread_id,
            writes.len()
        );
        Ok(())
    })
    .await
    .map_err(|e| sqlite_err("join blocking put_writes task", e))?
}

pub(super) async fn get_writes(
    conn: &Arc<Mutex<Connection>>,
    config: &CheckpointConfig,
    checkpoint_id: &str,
) -> Result<Vec<PendingWrite>> {
    let namespace_json =
        serde_json::to_string(&config.namespace).map_err(|e| sqlite_err("encode namespace", e))?;
    let conn = conn.clone();
    let thread_id = config.thread_id.clone();
    let checkpoint_id = checkpoint_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<Vec<PendingWrite>> {
        let conn = lock_conn(&conn)?;
        let mut stmt = conn
            .prepare(
                "SELECT node, task_id, idx, channel, payload FROM checkpoint_writes
                 WHERE thread_id = ?1 AND namespace = ?2 AND checkpoint_id = ?3
                 ORDER BY rowid ASC",
            )
            .map_err(|e| sqlite_err("prepare get_writes", e))?;
        let rows = stmt
            .query_map(
                params![thread_id, namespace_json, checkpoint_id],
                map_write_row,
            )
            .map_err(|e| sqlite_err("query get_writes", e))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| sqlite_err("read write row", e))??);
        }
        Ok(out)
    })
    .await
    .map_err(|e| sqlite_err("join blocking get_writes task", e))?
}

/// Decodes one `checkpoint_writes` row into a [`PendingWrite`].
///
/// Returns a nested `Result` because the payload decode can fail with a serde
/// error that `rusqlite`'s row-mapper signature has no room for.
#[allow(clippy::type_complexity)]
fn map_write_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<PendingWrite>> {
    let node: String = row.get(0)?;
    let task_id: String = row.get(1)?;
    let idx: i64 = row.get(2)?;
    let channel: String = row.get(3)?;
    let payload_json: String = row.get(4)?;
    Ok(
        match serde_json::from_str::<serde_json::Value>(&payload_json) {
            Ok(payload) => Ok(PendingWrite {
                node: NodeId::from(node),
                task_id,
                idx,
                channel,
                payload,
            }),
            Err(e) => Err(sqlite_err("decode write payload", e)),
        },
    )
}

/// Reads every write in `thread_id`/`namespace`, grouped by checkpoint id.
///
/// One query for the whole lineage, so `state_history` does not issue a
/// `get_writes` per hop. Used by [`super::reads::state_history`].
pub(super) fn read_writes_by_checkpoint(
    conn: &Connection,
    thread_id: &str,
    namespace_json: &str,
) -> Result<std::collections::HashMap<String, Vec<PendingWrite>>> {
    let mut stmt = conn
        .prepare(
            "SELECT checkpoint_id, node, task_id, idx, channel, payload FROM checkpoint_writes
             WHERE thread_id = ?1 AND namespace = ?2 ORDER BY rowid ASC",
        )
        .map_err(|e| sqlite_err("prepare writes-by-checkpoint", e))?;
    let rows = stmt
        .query_map(params![thread_id, namespace_json], |row| {
            let checkpoint_id: String = row.get(0)?;
            let node: String = row.get(1)?;
            let task_id: String = row.get(2)?;
            let idx: i64 = row.get(3)?;
            let channel: String = row.get(4)?;
            let payload_json: String = row.get(5)?;
            Ok((checkpoint_id, node, task_id, idx, channel, payload_json))
        })
        .map_err(|e| sqlite_err("query writes-by-checkpoint", e))?;
    let mut out: std::collections::HashMap<String, Vec<PendingWrite>> =
        std::collections::HashMap::new();
    for row in rows {
        let (checkpoint_id, node, task_id, idx, channel, payload_json) =
            row.map_err(|e| sqlite_err("read write row", e))?;
        let payload = serde_json::from_str(&payload_json)
            .map_err(|e| sqlite_err("decode write payload", e))?;
        let write = PendingWrite {
            node: NodeId::from(node),
            task_id,
            idx,
            channel,
            payload,
        };
        let slot = out.entry(checkpoint_id).or_default();
        // `merge_writes` keeps the shared dedupe semantics even here, where the
        // primary key already guarantees uniqueness — one rule, one place.
        merge_writes(slot, std::slice::from_ref(&write));
    }
    Ok(out)
}
