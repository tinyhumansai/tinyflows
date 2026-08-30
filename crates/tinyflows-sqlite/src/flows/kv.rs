//! The generic namespaced key/value table a host binds to
//! `tinyflows::caps::StateStore`.

use anyhow::{Context, Result};
use rusqlite::params;
use std::path::Path;

use super::{sql_conversion_error, with_connection};

/// Loads a value from the `flow_state` KV table, scoped to `namespace`.
///
/// Backs `tinyflows::caps::StateStore::load` via
/// a host's `tinyflows::caps::StateStore` binding.
pub fn kv_get(dir: &Path, namespace: &str, key: &str) -> Result<Option<serde_json::Value>> {
    with_connection(dir, |conn| {
        let mut stmt =
            conn.prepare("SELECT value FROM flow_state WHERE namespace = ?1 AND key = ?2")?;
        let mut rows = stmt.query(params![namespace, key])?;
        match rows.next()? {
            Some(row) => {
                let raw: String = row.get(0)?;
                let value: serde_json::Value =
                    serde_json::from_str(&raw).map_err(sql_conversion_error)?;
                Ok(Some(value))
            }
            None => Ok(None),
        }
    })
}

/// Stores a value into the `flow_state` KV table, scoped to `namespace`.
///
/// Backs `tinyflows::caps::StateStore::store` via
/// a host's `tinyflows::caps::StateStore` binding.
pub fn kv_set(dir: &Path, namespace: &str, key: &str, value: &serde_json::Value) -> Result<()> {
    let raw = serde_json::to_string(value).context("Failed to serialize flow state value")?;
    with_connection(dir, |conn| {
        conn.execute(
            "INSERT INTO flow_state (namespace, key, value) VALUES (?1, ?2, ?3)
             ON CONFLICT(namespace, key) DO UPDATE SET value = excluded.value",
            params![namespace, key, raw],
        )
        .context("Failed to store flow state value")?;
        Ok(())
    })
}

/// Deletes one key from the `flow_state` KV table, scoped to `namespace`.
/// A no-op (not an error) when the key doesn't exist.
///
/// Used by `flows::bus::DedupCommitSubscriber` (issue #5263 PR2) to clear a
/// `dedup` node's `tentative` key set once a run's outcome has been settled —
/// preferred over `kv_set(.., json!([]))` because an absent key reads back as
/// `None` (an unambiguous "nothing pending"), matching what a fresh flow that
/// never ran a dedup node also reads back as.
pub fn kv_delete(dir: &Path, namespace: &str, key: &str) -> Result<()> {
    with_connection(dir, |conn| {
        conn.execute(
            "DELETE FROM flow_state WHERE namespace = ?1 AND key = ?2",
            params![namespace, key],
        )
        .context("Failed to delete flow state value")?;
        Ok(())
    })
}

#[cfg(test)]
#[path = "kv_tests.rs"]
mod tests;
