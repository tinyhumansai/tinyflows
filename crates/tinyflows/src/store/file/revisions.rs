//! Superseded copies of a workflow, so an edit can be taken back.
//!
//! The copilot writes to the store directly — that is the design, and it is why
//! every authoring surface shares one path — but it left an operator with no way
//! to disagree with an edit after the fact. A harness turn that misread the
//! instruction rewrote the graph and the previous one was gone.
//!
//! So [`capture`] runs inside [`super::FileWorkflowStore::save`] and
//! [`super::FileWorkflowStore::delete`], snapshotting what is about to be
//! replaced. Being in the store rather than at the call sites is the point:
//! the copilot, the host's `workflow` subcommand, and the MCP tools all become
//! undoable without any of them knowing revisions exist.
//!
//! Snapshots live in the workflow state directory. They are host history rather
//! than authored source, so syncing the definitions never pulls along undo
//! state. They are capped at [`MAX_REVISIONS`] per workflow: history is for
//! taking back a mistake that was just made, not an archive.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::store::types::{WorkflowError, WorkflowRecord, WorkflowRevision};

use super::paths::{is_json, safe_component, write_atomic};

/// How many superseded copies of one workflow are kept.
///
/// Matches a sibling host's cap. Past this, an operator is not
/// undoing an edit they just watched happen — they want the version control the
/// project is already in.
pub const MAX_REVISIONS: usize = 20;

/// Tie-breaker for snapshots taken inside the same millisecond.
///
/// Process-wide rather than per-workflow: it only has to be increasing, and one
/// counter cannot be raced into reuse the way a per-directory count read off the
/// filesystem could.
static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A snapshot as it is stored: the whole record, plus when it stopped being
/// current.
///
/// The record is embedded rather than flattened so a revision keeps loading if
/// the record shape gains a field.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredRevision {
    /// Epoch-millisecond stamp of when this copy was *superseded*.
    superseded_at: u64,
    /// The workflow as it was.
    record: WorkflowRecord,
}

/// Where one workflow's snapshots live.
fn dir_for(revisions_dir: &Path, workflow_id: &str) -> Result<PathBuf, WorkflowError> {
    Ok(revisions_dir.join(safe_component(workflow_id)?))
}

/// Snapshot `record` as a superseded version, then prune to [`MAX_REVISIONS`].
///
/// # Errors
///
/// Fails when the id is not a usable filename, or when the snapshot cannot be
/// written — the caller is about to overwrite the only copy, so a history it
/// could not record is a failure rather than something to log past.
pub fn capture(write_dir: &Path, record: &WorkflowRecord) -> Result<PathBuf, WorkflowError> {
    let dir = dir_for(write_dir, &record.id)?;
    let superseded_at = now_ms();
    // Three parts, and each earns its place. The zero-padded stamp leads so a
    // lexical sort is a chronological one. A monotonic counter follows it
    // because several saves land inside one millisecond — a burst of copilot
    // edits does — and without it the sort would fall through to the random
    // token, making both the listing order and *which* revision the cap drops
    // arbitrary. The random token is last, and only for uniqueness: two
    // processes can pick the same counter.
    let revision_id = format!(
        "{superseded_at:013}-{:012}-{}",
        SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        crate::ids::token()
    );
    let stored = StoredRevision {
        superseded_at,
        record: WorkflowRecord {
            // The path a record was read from is where it lived, not part of
            // what it was; carrying it into a snapshot would make a rollback
            // claim to have come from a file that holds something else.
            source_path: None,
            ..record.clone()
        },
    };
    let body = serde_json::to_vec_pretty(&stored)
        .map_err(|err| WorkflowError::Malformed(err.to_string()))?;
    let path = dir.join(format!("{revision_id}.json"));
    write_atomic(&path, &body)?;
    Ok(path)
}

/// Commit a captured snapshot after its matching source mutation succeeds.
///
/// Pruning is housekeeping, not part of the commit: the definition write or
/// delete this follows has already landed, so a pruning failure (for example
/// `read_dir` failing on the revisions directory) must not turn an already-
/// successful save into a reported failure — a caller that saw `Err` here
/// would retry the whole mutation and record a second, spurious revision.
pub fn commit_capture(path: &Path) -> Result<(), WorkflowError> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    if let Err(error) = prune(dir) {
        tracing::warn!(path = %dir.display(), %error, "failed to prune workflow revisions");
    }
    Ok(())
}

/// Remove a snapshot whose matching source mutation failed.
pub fn rollback_capture(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// Every snapshot of `workflow_id`, newest first.
///
/// A snapshot this host cannot parse is skipped rather than failing the listing,
/// matching how run history already behaves: one corrupt file should not hide
/// the rest of it.
pub fn list(write_dir: &Path, workflow_id: &str) -> Result<Vec<WorkflowRevision>, WorkflowError> {
    let dir = dir_for(write_dir, workflow_id)?;
    let mut revisions: Vec<WorkflowRevision> = snapshot_paths(&dir)?
        .into_iter()
        .filter_map(|path| load(&path).ok().flatten())
        .collect();
    revisions.sort_by(|a, b| b.id.cmp(&a.id));
    Ok(revisions)
}

/// Merge snapshots from the current state directory and a legacy directory.
///
/// Revision identifiers are globally unique in normal operation. Deduplicating
/// them also makes a partially migrated history harmless.
pub(super) fn list_merged(
    current_dir: &Path,
    legacy_dir: &Path,
    workflow_id: &str,
) -> Result<Vec<WorkflowRevision>, WorkflowError> {
    let mut revisions = list(current_dir, workflow_id)?;
    revisions.extend(list(legacy_dir, workflow_id)?);
    revisions.sort_by(|a, b| b.id.cmp(&a.id));
    revisions.dedup_by(|a, b| a.id == b.id);
    revisions.truncate(MAX_REVISIONS);
    Ok(revisions)
}

/// One snapshot by id, scoped to its workflow.
///
/// Scoped deliberately: a revision id is enough to name a file, and letting one
/// workflow's rollback reach another's history would be a way to write a graph
/// an operator never had.
pub fn read(
    write_dir: &Path,
    workflow_id: &str,
    revision_id: &str,
) -> Result<Option<WorkflowRevision>, WorkflowError> {
    let path =
        dir_for(write_dir, workflow_id)?.join(format!("{}.json", safe_component(revision_id)?));
    load(&path)
}

/// Read one snapshot file, treating absence as `None`.
fn load(path: &Path) -> Result<Option<WorkflowRevision>, WorkflowError> {
    let body = match std::fs::read(path) {
        Ok(body) => body,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(WorkflowError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let stored: StoredRevision = serde_json::from_slice(&body)
        .map_err(|err| WorkflowError::Malformed(format!("{}: {err}", path.display())))?;
    let id = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or_default()
        .to_string();
    Ok(Some(WorkflowRevision {
        id,
        superseded_at: stored.superseded_at,
        record: stored.record,
    }))
}

/// Every snapshot file in `dir`, sorted oldest first. A missing directory is the
/// normal state for a workflow that has never been edited.
fn snapshot_paths(dir: &Path) -> Result<Vec<PathBuf>, WorkflowError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(WorkflowError::Io {
                path: dir.to_path_buf(),
                source,
            });
        }
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| is_json(path))
        .collect();
    // Filenames lead with a zero-padded stamp, so this is chronological.
    paths.sort();
    Ok(paths)
}

/// Drop the oldest snapshots past [`MAX_REVISIONS`].
///
/// A snapshot that cannot be removed is not an error: the cap is housekeeping,
/// and failing the *save* that triggered it would be a worse outcome than one
/// extra file on disk.
fn prune(dir: &Path) -> Result<(), WorkflowError> {
    let paths = snapshot_paths(dir)?;
    let excess = paths.len().saturating_sub(MAX_REVISIONS);
    for path in paths.into_iter().take(excess) {
        let _ = std::fs::remove_file(path);
    }
    Ok(())
}

/// Epoch milliseconds, saturating at zero if the clock is before the epoch.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

#[cfg(test)]
#[path = "revisions_tests.rs"]
mod tests;
