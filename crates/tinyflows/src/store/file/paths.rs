//! Turning identifiers into filenames, and writing files without tearing.
//!
//! Everything here guards the boundary between a name this host was *given* and
//! a path it will *act on*. Workflow ids, run ids, and revision ids all arrive
//! from somewhere less trusted than this process — a document an agent wrote, a
//! task frame from a peer — and all three become filenames.

use std::path::{Path, PathBuf};

use crate::store::types::WorkflowError;

/// Suffix appended while writing, then renamed over the target. Matches the
/// idiom already used for trust state, so a half-written file is never
/// observable — and never mistaken for a definition, since the resulting
/// extension is not `json`.
const TMP_SUFFIX: &str = ".flows-tmp";

/// An identifier's use as a single filename component, or an error.
///
/// Workflow ids and run ids both become filenames, and both are attacker-shaped
/// input: a workflow document's `id` overrides whatever the caller asked for, a
/// document may be written by an agent, and a run id can arrive on a task frame
/// from a peer. Without this, an id of `../../authorized_keys` would let a save
/// write outside the workflow directory with the daemon's privileges.
///
/// The rule is deliberately strict rather than sanitizing: an id that is not
/// already a safe component is rejected, not silently rewritten into a
/// different one. Rewriting would let two distinct ids collapse onto one file.
pub fn safe_component(id: &str) -> Result<&str, WorkflowError> {
    if id.trim().is_empty() {
        return Err(WorkflowError::Malformed(
            "identifier must not be empty".to_string(),
        ));
    }
    // Rejected rather than rewritten: silently trimming would let "sweep " and
    // "sweep" collapse onto the same `sweep.json`, and the stored record would
    // still carry the untrimmed id — no longer matching the filename it
    // resolves to.
    if id.trim() != id {
        return Err(WorkflowError::Malformed(format!(
            "identifier '{id}' must not have leading or trailing whitespace"
        )));
    }
    let trimmed = id;
    if trimmed == "." || trimmed == ".." {
        return Err(WorkflowError::Malformed(format!(
            "identifier '{trimmed}' is not a usable filename"
        )));
    }
    // Both separators, on every platform: a document written on one machine is
    // read on another, and `a\..\b` must not become traversal on Windows just
    // because it was authored on unix.
    if trimmed.contains('/') || trimmed.contains('\\') || trimmed.contains('\0') {
        return Err(WorkflowError::Malformed(format!(
            "identifier '{trimmed}' must not contain a path separator"
        )));
    }
    // Catches drive-relative and other platform spellings the checks above miss
    // by asking the platform itself whether this is one plain component.
    let path = Path::new(trimmed);
    let mut components = path.components();
    match (components.next(), components.next()) {
        (Some(std::path::Component::Normal(_)), None) => Ok(trimmed),
        _ => Err(WorkflowError::Malformed(format!(
            "identifier '{trimmed}' must be a single path component"
        ))),
    }
}

/// Whether a path is a file this store reads.
pub fn is_json(path: &Path) -> bool {
    path.is_file()
        && path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("json"))
            .unwrap_or(false)
}

/// Write `body` to `path` through a temporary file in the same directory, so a
/// reader never observes a half-written document.
pub fn write_atomic(path: &Path, body: &[u8]) -> Result<(), WorkflowError> {
    stage_atomic(path, body)?.commit()
}

/// A complete temporary write waiting to be renamed over its destination.
pub struct StagedWrite {
    tmp: PathBuf,
    path: PathBuf,
}

impl StagedWrite {
    /// Publish the staged bytes atomically.
    pub fn commit(self) -> Result<(), WorkflowError> {
        std::fs::rename(&self.tmp, &self.path).map_err(|source| WorkflowError::Io {
            path: self.path.clone(),
            source,
        })
    }
}

impl Drop for StagedWrite {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.tmp);
    }
}

/// Write all bytes into the target directory without replacing the target yet.
pub fn stage_atomic(path: &Path, body: &[u8]) -> Result<StagedWrite, WorkflowError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| WorkflowError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    // Appended rather than substituted for the extension, so an id containing a
    // dot cannot collide with a different workflow's temporary file — and
    // carrying a unique token, so two writers racing on the *same* id cannot
    // scribble over each other's scratch file before either rename lands.
    let mut tmp_name = path.as_os_str().to_os_string();
    tmp_name.push(format!("{TMP_SUFFIX}.{}", crate::ids::token()));
    let tmp = PathBuf::from(tmp_name);
    std::fs::write(&tmp, body).map_err(|source| WorkflowError::Io {
        path: tmp.clone(),
        source,
    })?;
    Ok(StagedWrite {
        tmp,
        path: path.to_path_buf(),
    })
}
