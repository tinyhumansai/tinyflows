//! Locks, recovers, reads, and atomically writes one workflow's journal.

use std::path::{Path, PathBuf};

use fs2::FileExt;

use crate::store::types::{WorkflowError, WorkflowNote};

use super::super::paths::{safe_component, write_atomic};
use super::prune::prune;

/// Where one workflow's journal lives.
fn path_for(journal_dir: &Path, workflow_id: &str) -> Result<PathBuf, WorkflowError> {
    Ok(journal_dir.join(format!("{}.json", safe_component(workflow_id)?)))
}

/// Every note for `workflow_id`, newest first, superseded ones included.
///
/// A journal this host cannot parse yields an empty list with a warning rather
/// than an error. The alternative is that one bad file makes a workflow
/// unreadable everywhere its notes are shown, which is a worse failure than
/// forgetting what it learned — and run history already behaves this way.
pub fn list(journal_dir: &Path, workflow_id: &str) -> Result<Vec<WorkflowNote>, WorkflowError> {
    with_write_lock(journal_dir, workflow_id, || {
        let mut notes = read_all(journal_dir, workflow_id)?;
        notes.sort_by(|a, b| b.id.cmp(&a.id));
        Ok(notes)
    })
}

/// Append `note`, then prune to [`super::MAX_NOTES`].
///
/// # Errors
///
/// Fails when the workflow id is not a usable filename, or when the file cannot
/// be written. A note that could not be recorded is a real failure: the callers
/// that append are the ones claiming the host now knows something.
pub fn append(journal_dir: &Path, note: &WorkflowNote) -> Result<(), WorkflowError> {
    with_write_lock(journal_dir, &note.workflow_id, || {
        let mut notes = read_all(journal_dir, &note.workflow_id)?;
        notes.push(note.clone());
        prune(&mut notes);
        write(journal_dir, &note.workflow_id, &notes)
    })
}

/// Mark `id` as replaced by `by`, returning whether a current note changed.
///
/// Silently does nothing when the note is not there. Supersession is a tidying
/// action taken after the fact, and a caller naming a note that has already
/// been pruned away has nothing left to fix.
pub fn supersede(
    journal_dir: &Path,
    workflow_id: &str,
    id: &str,
    by: &str,
) -> Result<bool, WorkflowError> {
    with_write_lock(journal_dir, workflow_id, || {
        let mut notes = read_all(journal_dir, workflow_id)?;
        let mut changed = false;
        for note in notes.iter_mut() {
            if note.id == id && note.superseded_by.is_none() {
                note.superseded_by = Some(by.to_string());
                changed = true;
            }
        }
        if !changed {
            return Ok(false);
        }
        write(journal_dir, workflow_id, &notes)?;
        Ok(true)
    })
}

/// Serialize journal reads and writes across stores and processes.
///
/// Reads participate because recovering a corrupt file renames it. Without the
/// same lock a reader could quarantine the valid replacement a writer had just
/// installed after the reader captured the old corrupt bytes.
fn with_write_lock<T>(
    journal_dir: &Path,
    workflow_id: &str,
    write_operation: impl FnOnce() -> Result<T, WorkflowError>,
) -> Result<T, WorkflowError> {
    std::fs::create_dir_all(journal_dir).map_err(|source| WorkflowError::Io {
        path: journal_dir.to_path_buf(),
        source,
    })?;
    let lock_path = journal_dir.join(format!("{}.lock", safe_component(workflow_id)?));
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|source| WorkflowError::Io {
            path: lock_path.clone(),
            source,
        })?;
    lock.lock_exclusive().map_err(|source| WorkflowError::Io {
        path: lock_path.clone(),
        source,
    })?;
    let result = write_operation();
    // Qualified rather than `lock.unlock()`: `std::fs::File` grew an inherent
    // `unlock` in 1.89, which would win the method lookup and take this crate
    // past its 1.85 MSRV without any error saying so.
    if let Err(source) = FileExt::unlock(&lock) {
        tracing::warn!(path = %lock_path.display(), "failed to release journal lock: {source}");
    }
    result
}

/// Read the file, treating absence and corruption alike as "nothing learned".
fn read_all(journal_dir: &Path, workflow_id: &str) -> Result<Vec<WorkflowNote>, WorkflowError> {
    let path = path_for(journal_dir, workflow_id)?;
    let body = match std::fs::read(&path) {
        Ok(body) => body,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(WorkflowError::Io { path, source }),
    };
    match serde_json::from_slice::<Vec<WorkflowNote>>(&body) {
        Ok(notes) => Ok(notes),
        Err(err) => {
            // Kept, not overwritten. Reading past a corrupt journal is a
            // deliberate kindness; appending on top of it would destroy
            // whatever an operator might still have recovered by hand, which
            // is a different and much less forgivable thing to do.
            let quarantine = path.with_extension(format!("json.corrupt.{}", crate::ids::token()));
            let _ = std::fs::rename(&path, &quarantine);
            tracing::warn!(
                workflow = %workflow_id,
                path = %path.display(),
                kept = %quarantine.display(),
                "workflow journal is unreadable; moved aside and starting a new one: {err}"
            );
            Ok(Vec::new())
        }
    }
}

/// Write the whole journal back.
fn write(
    journal_dir: &Path,
    workflow_id: &str,
    notes: &[WorkflowNote],
) -> Result<(), WorkflowError> {
    let body = serde_json::to_vec_pretty(notes)
        .map_err(|err| WorkflowError::Malformed(err.to_string()))?;
    write_atomic(&path_for(journal_dir, workflow_id)?, &body)
}
