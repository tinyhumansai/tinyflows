//! JSON workflow documents under the host's data directory, one graph per file.
//!
//! The directory layering, the forgiving read, and the atomic write all match
//! how agent templates are already kept ([`crate::agents`]) — an operator who
//! has learned one has learned the other. The format is JSON rather than the
//! TOML used for templates because a node's `config` is free-form JSON that the
//! engine hands to jq expressions; round-tripping it through TOML would change
//! what the author wrote.
//!
//! Reading never fails as a whole. A missing directory is the normal state, and
//! a malformed document costs only itself — an operator hand-editing a catalog
//! should lose the file they broke, not the nine that are fine. What went wrong
//! travels back in [`LoadReport::errors`].
//!
//! The work is split by responsibility: [`dirs`] decides where to look,
//! [`document`] turns bytes into a record and back, [`paths`] guards the
//! identifier-to-filename boundary, and [`revisions`] keeps the superseded
//! copies that make an edit undoable. This module is the store itself.

mod dirs;
mod document;
mod journal;
mod paths;
mod proposals;
mod revisions;

pub use dirs::workflow_dirs;
pub use document::{
    EnginePolicy, HostPolicy, gate_failures_into_error, new_run_record, parse_workflow,
    parse_workflow_with, read_workflow, read_workflow_with, validate_graph,
};
pub use journal::{MAX_NOTES, mint_id as mint_note_id};
pub use proposals::mint_id as mint_proposal_id;
pub use revisions::MAX_REVISIONS;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use fs2::FileExt;
use sha2::{Digest, Sha256};

use crate::store::types::{
    RunRecord, WorkflowError, WorkflowNote, WorkflowProposal, WorkflowRecord, WorkflowRevision,
    WorkflowSummary,
};

use dirs::catalog_identity;
pub(super) use dirs::definition_state_dir;
use document::to_document;
pub use paths::write_atomic;
use paths::{is_json, stage_atomic};
// Re-exported within the crate rather than merely imported: the identifier
// guard is the one piece of this module worth asserting on from outside it.
pub use paths::safe_component;

use super::{ProposalDecisionGuard, WorkflowStore};

/// The host-state directory this environment and working directory resolve to.
///
/// Everything a host records *about* workflows rather than as part of them —
/// runs, journal notes, proposals, host transcripts — hangs off this one path,
/// scoped to the workspace so two checkouts of the same repository do not read
/// each other's history. Exposed rather than left inside
/// [`FileWorkflowStore::discover`] because a host keeping its own per-workflow
/// state (a transcript store, say) has to land it in the same place, and a
/// second copy of this derivation is a second thing to keep in step.
///
/// `home` is the host's own persistent-data root, passed in rather than derived
/// here: a host that resolved it once at startup — honouring a `--home` flag, or
/// a test fixture's scratch directory — must not have it re-derived from the
/// process environment behind its back.
pub fn workspace_state_dir_under(home: &Path, cwd: &Path) -> PathBuf {
    scoped_state_dir(&home.join("state").join("workflows"), cwd)
}

/// `state_dir` narrowed to one workspace, by a digest of its canonical path.
fn scoped_state_dir(state_dir: &Path, workspace: &Path) -> PathBuf {
    state_dir.join("scopes").join(workspace_scope(workspace))
}

/// The directory-name digest that identifies one workspace.
///
/// Canonical rather than literal so `.` and a symlinked checkout resolve to the
/// same scope; truncated to sixteen hex characters because this is a directory
/// name a person occasionally has to read, and collision here would need a
/// deliberate preimage attack on a path nobody else chooses.
///
/// Shared in-crate because the generated skills are scoped the same way and by
/// the same rule — a second copy of this derivation is a second thing that can
/// drift.
pub fn workspace_scope(workspace: &Path) -> String {
    let identity = std::fs::canonicalize(workspace).unwrap_or_else(|_| absolute_path(workspace));
    let digest = Sha256::digest(identity.to_string_lossy().as_bytes());
    format!("{digest:x}")[..16].to_string()
}

/// A file-backed proposal decision claim released when dropped.
struct FileProposalDecisionGuard {
    file: std::fs::File,
    path: PathBuf,
}

impl ProposalDecisionGuard for FileProposalDecisionGuard {}

impl Drop for FileProposalDecisionGuard {
    fn drop(&mut self) {
        if let Err(source) = FileExt::unlock(&self.file) {
            tracing::warn!(path = %self.path.display(), "failed to release proposal decision lock: {source}");
        }
    }
}

/// What one read of the workflow directories found.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LoadReport {
    /// Workflows in load order, later directories having replaced earlier ones
    /// of the same id.
    pub workflows: Vec<WorkflowRecord>,
    /// Directories that existed and were read, in precedence order.
    pub dirs: Vec<PathBuf>,
    /// One message per document that could not be read, parsed, or validated.
    pub errors: Vec<String>,
}

/// A workflow store backed by JSON files in the layered workflow directories.
#[derive(Debug, Clone)]
pub struct FileWorkflowStore {
    /// Definition directories, lowest precedence first.
    dirs: Vec<PathBuf>,
    /// Where run records are written. Runs are host state, not an authored
    /// artifact, so they live under the state directory rather than beside the
    /// definitions an operator edits.
    runs_dir: PathBuf,
    /// Where per-workflow notes are written.
    ///
    /// Beside the runs rather than beside the definitions for the same reason:
    /// a journal is what this host observed while running the workflow, not
    /// part of the document an operator edits and commits.
    journal_dir: PathBuf,
    /// Where proposed graph changes are written, awaiting an operator.
    proposals_dir: PathBuf,
    /// Where superseded workflow definitions are kept for undo.
    revisions_dir: PathBuf,
    /// Where cross-process definition locks live.
    ///
    /// Locks are runtime coordination, so they must not appear among authored
    /// files an operator may sync between machines.
    definition_locks_dir: PathBuf,
    /// Stable identity for in-process decisions and evolution claims.
    ///
    /// Derived from the persistent proposal directory rather than this
    /// object's address because daemon tasks construct independent store
    /// instances over the same on-disk state.
    decision_scope: String,
    /// Serializes `save`/`delete` against each other on *this store instance*.
    ///
    /// Both are read-modify-write: read what a save would supersede or what a
    /// delete would remove, capture that as a revision, then write. Two
    /// concurrent writers for the same id — a copilot autosave racing a manual
    /// TUI edit, both holding a `clone()` of this store — could otherwise
    /// interleave those steps and either lose one edit's revision snapshot or
    /// have one silently overwrite the other's write with a stale read. `Arc`
    /// so every clone of this store shares the one lock rather than each
    /// getting its own and serializing nothing.
    ///
    /// Separate store instances and processes additionally synchronize through
    /// the per-workflow file lock acquired by every definition writer.
    write_lock: Arc<Mutex<()>>,
    /// The host rules this store judges documents and edits by.
    ///
    /// The engine's own unless a host sets one with
    /// [`FileWorkflowStore::with_policy`]: the engine cannot judge a harness
    /// name it has never heard of, but the host that owns the vocabulary can,
    /// and a bad one must fail at load rather than at dispatch.
    policy: Arc<dyn HostPolicy>,
}

mod store_impl;

/// Make a stable best-effort absolute identity when a path does not yet exist.
fn absolute_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(path)
}

/// A stable process-local key for every store instance over `proposals_dir`.
fn file_store_scope(proposals_dir: &Path) -> String {
    format!("file:{}", absolute_path(proposals_dir).to_string_lossy())
}

mod workflow_store_impl;

/// Add `record` to `workflows`, replacing any entry with the same id in place so
/// a project-local override keeps the position of what it overrides.
fn upsert(workflows: &mut Vec<WorkflowRecord>, record: WorkflowRecord) {
    match workflows.iter_mut().find(|w| w.id == record.id) {
        Some(existing) => *existing = record,
        None => workflows.push(record),
    }
}
