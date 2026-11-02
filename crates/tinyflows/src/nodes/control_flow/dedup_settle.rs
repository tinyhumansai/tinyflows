//! The host half of the `dedup` node's commit-on-success contract, as pure
//! logic over a synchronous key/value seam.
//!
//! The [`DedupNode`](super::DedupNode) writes each unseen item's key into a
//! *tentative* set during a run, scoped to the run's id when the host seeds one
//! ([`tentative_key`]). When the run finishes, the host settles every `dedup`
//! node the run executed through [`settle`], with that same run id:
//!
//! - **success** — union `tentative` into `committed`, then clear `tentative`
//!   ([`commit`]). If that fails for a run-scoped set, its keys are parked in
//!   the node's carryover set and the next successful commit folds them in;
//! - **anything else** — clear `tentative` only ([`release`]), so the keys are
//!   exactly as unseen as before the run and the next run reprocesses them.
//!
//! The host keeps everything with a lifecycle: the run-finished subscriber, the
//! per-flow lock that serializes concurrent settlements (this is a
//! read-modify-write, not a compare-and-swap), and which flow's graph names the
//! `dedup` nodes. This module only knows the key layout and the set algebra.

use std::collections::HashSet;

use serde_json::Value;

use super::dedup::{committed_key, tentative_key};

/// Synchronous key/value store scoped to one flow's state namespace: the same
/// namespace the engine's `StateStore` hands the `dedup` node during the run.
pub trait DedupKv {
    /// Loads the value under `key`; `Ok(None)` when absent.
    fn kv_get(&self, key: &str) -> Result<Option<Value>, String>;
    /// Stores `value` under `key`.
    fn kv_set(&self, key: &str, value: &Value) -> Result<(), String>;
    /// Deletes `key`; a missing key is not an error.
    fn kv_delete(&self, key: &str) -> Result<(), String>;
}

/// How [`commit`] ended for one node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitOutcome {
    /// There were no tentative keys; nothing was written.
    NothingTentative,
    /// A stored set could not be read, or the committed set could not be
    /// written. `committed` is untouched and the tentative keys are kept for
    /// the next successful run to retry: the shared set stays in place, and a
    /// run-scoped set (which no later run settles) is parked in the node's
    /// carryover set.
    CommitFailed(String),
    /// Tentative keys were unioned into `committed`.
    Committed {
        /// Keys newly added (tentative keys not already committed).
        added: usize,
        /// Size of the committed set after the union.
        committed_len: usize,
        /// `Some(error)` when clearing `tentative` failed afterwards. Harmless:
        /// the committed set is idempotent under re-union.
        clear_error: Option<String>,
    },
}

/// What [`settle`] did for one node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Settlement {
    /// The run succeeded; see the inner [`CommitOutcome`].
    Commit(CommitOutcome),
    /// The run did not succeed; `Err` carries a failed `tentative` delete
    /// (harmless: `committed` is untouched either way).
    Release(Result<(), String>),
}

/// Whether a finished run's status counts as success for settlement.
/// `completed_with_warnings` does; `failed`, `cancelled`, `interrupted` and any
/// unrecognized status do not ("retry an already-done item" is always safe,
/// "mark an uncertain outcome done" is not).
#[must_use]
pub fn is_success_status(status: &str) -> bool {
    matches!(status, "completed" | "completed_with_warnings")
}

/// Settles one `dedup` node for run `run_id`: [`commit`] when `success`, else
/// [`release`]. `run_id` must be the id the host seeded as
/// [`RunInput::run_id`](crate::engine::RunInput::run_id) for that run (`None`
/// when it seeded none), or the node's tentative keys are not found.
pub fn settle(kv: &dyn DedupKv, node_id: &str, run_id: Option<&str>, success: bool) -> Settlement {
    if success {
        Settlement::Commit(commit(kv, node_id, run_id))
    } else {
        Settlement::Release(release(kv, node_id, run_id))
    }
}

/// Success path: union run `run_id`'s `tentative` set (plus any carryover a
/// failed run-scoped commit parked) into `committed`, then clear both.
pub fn commit(kv: &dyn DedupKv, node_id: &str, run_id: Option<&str>) -> CommitOutcome {
    let tentative_key = tentative_key(node_id, run_id);
    let committed_key = committed_key(node_id);
    let carryover_key = carryover_key(node_id);

    let tentative = match load_key_set(kv, &tentative_key) {
        Ok(set) => set,
        Err(e) => return CommitOutcome::CommitFailed(e),
    };
    // Read the carryover whether or not this run is scoped: a run-scoped
    // commit may have parked keys before the host stopped seeding run ids.
    // An unreadable carryover must not block this run's commit; it stays in
    // place (`None`) for a later run, and is never overwritten.
    let carryover = load_key_set(kv, &carryover_key).ok();
    // Only a run-scoped set is parked on failure: the shared set stays put.
    let park_key = run_id.map(|_| carryover_key.clone());
    if tentative.is_empty() && carryover.as_ref().is_none_or(HashSet::is_empty) {
        return CommitOutcome::NothingTentative;
    }

    // A failed committed read must not be treated as an empty set: writing
    // only the tentative keys back would forget every previously committed
    // key and let those items through the dedup again.
    let mut committed = match load_key_set(kv, &committed_key) {
        Ok(set) => set,
        Err(e) => {
            return park_for_retry(kv, &tentative_key, park_key, tentative, carryover, e);
        }
    };
    let added = tentative
        .iter()
        .chain(carryover.iter().flatten())
        .filter(|k| committed.insert((*k).clone()))
        .count();

    if let Err(e) = store_key_set(kv, &committed_key, &committed) {
        return park_for_retry(kv, &tentative_key, park_key, tentative, carryover, e);
    }
    let mut clear_error = kv.kv_delete(&tentative_key).err();
    if carryover.as_ref().is_some_and(|set| !set.is_empty())
        && let Err(e) = kv.kv_delete(&carryover_key)
    {
        clear_error.get_or_insert(e);
    }
    CommitOutcome::Committed {
        added,
        committed_len: committed.len(),
        clear_error,
    }
}

/// The key where [`commit`] parks a run-scoped tentative set whose commit
/// failed. Once its run is gone nothing would settle that run's own key again,
/// so the node's next successful commit picks the keys up from here instead.
fn carryover_key(node_id: &str) -> String {
    format!("dedup:{node_id}:uncommitted")
}

/// Moves a run-scoped `tentative` set into the readable carryover set after a
/// failed commit. Best effort: if the carryover is unreadable or cannot be
/// written, the run's tentative key is left as it was.
fn park_for_retry(
    kv: &dyn DedupKv,
    tentative_key: &str,
    carryover_key: Option<String>,
    tentative: HashSet<String>,
    carryover: Option<HashSet<String>>,
    error: String,
) -> CommitOutcome {
    if let (Some(key), Some(mut carryover)) = (carryover_key, carryover)
        && !tentative.is_empty()
    {
        carryover.extend(tentative);
        if store_key_set(kv, &key, &carryover).is_ok() {
            let _ = kv.kv_delete(tentative_key);
        }
    }
    CommitOutcome::CommitFailed(error)
}

/// Failure path: clear run `run_id`'s `tentative` set only, leaving
/// `committed` untouched. Does not read first: `kv_delete` already no-ops on a
/// missing key.
pub fn release(kv: &dyn DedupKv, node_id: &str, run_id: Option<&str>) -> Result<(), String> {
    kv.kv_delete(&tentative_key(node_id, run_id))
}

/// Loads a key set (a JSON array of strings). A missing key, non-array value,
/// or non-string elements degrade to an empty set: a first run against a fresh
/// store has nothing recorded, which is not a fault. A store error is
/// propagated, since treating it as empty would let [`commit`] overwrite the
/// real committed set.
fn load_key_set(kv: &dyn DedupKv, key: &str) -> Result<HashSet<String>, String> {
    Ok(match kv.kv_get(key) {
        Ok(Some(value)) => value
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        Ok(None) => HashSet::new(),
        Err(e) => {
            tracing::warn!(key, error = %e, "[dedup-commit] failed to load key set — aborting commit");
            return Err(e);
        }
    })
}

/// Persists `set` as a sorted JSON array of strings (stable, diffable).
fn store_key_set(kv: &dyn DedupKv, key: &str, set: &HashSet<String>) -> Result<(), String> {
    let mut keys: Vec<String> = set.iter().cloned().collect();
    keys.sort_unstable();
    let value = Value::Array(keys.into_iter().map(Value::String).collect());
    kv.kv_set(key, &value)
}

#[cfg(test)]
#[path = "dedup_settle_tests.rs"]
mod tests;
