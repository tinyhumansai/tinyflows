//! Pending graph changes on disk.
//!
//! One file per proposal, under the state directory beside runs and the
//! journal. Unlike notes, a proposal is read individually as often as in a set
//! — an operator accepts *this* one — so a file each keeps a decision from
//! rewriting every other proposal's record.
//!
//! Listing scans the directory, the same shape run history has. That is
//! acceptable here in a way it is not there: an evolution pass supersedes its
//! own undecided proposal rather than adding to a pile, so the directory stays
//! small by construction.

use std::path::{Path, PathBuf};

use crate::store::types::{ProposalId, WorkflowError, WorkflowProposal};

use super::paths::{is_json, safe_component, write_atomic};

/// Tie-breaker for proposals minted inside the same millisecond.
static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Mint a proposal id that sorts chronologically.
pub fn mint_id(created_at: u64) -> ProposalId {
    format!(
        "{created_at:013}-{:012}-{}",
        SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        crate::ids::token()
    )
}

/// Where one proposal lives.
fn path_for(proposals_dir: &Path, id: &str) -> Result<PathBuf, WorkflowError> {
    Ok(proposals_dir.join(format!("{}.json", safe_component(id)?)))
}

/// Write a proposal, replacing any earlier state for the same id.
///
/// Every state change — verified, accepted, rejected, made stale — goes through
/// here, so a proposal's file is always its current state rather than a log to
/// replay.
pub fn save(proposals_dir: &Path, proposal: &WorkflowProposal) -> Result<(), WorkflowError> {
    let body = serde_json::to_vec_pretty(proposal)
        .map_err(|err| WorkflowError::Malformed(err.to_string()))?;
    write_atomic(&path_for(proposals_dir, &proposal.id)?, &body)
}

/// One proposal by id, or `None` when there is no such file.
pub fn read(proposals_dir: &Path, id: &str) -> Result<Option<WorkflowProposal>, WorkflowError> {
    let path = path_for(proposals_dir, id)?;
    let body = match std::fs::read(&path) {
        Ok(body) => body,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(WorkflowError::Io { path, source }),
    };
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|err| WorkflowError::Malformed(format!("{}: {err}", path.display())))
}

/// Every proposal for one workflow, newest first.
///
/// A file this host cannot parse is skipped rather than failing the listing, so
/// one bad proposal does not hide the rest — the same bargain run history and
/// the journal already make.
pub fn list_for(
    proposals_dir: &Path,
    workflow_id: &str,
) -> Result<Vec<WorkflowProposal>, WorkflowError> {
    let entries = match std::fs::read_dir(proposals_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(WorkflowError::Io {
                path: proposals_dir.to_path_buf(),
                source,
            });
        }
    };
    let mut proposals: Vec<WorkflowProposal> = entries
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| is_json(path))
        .filter_map(|path| match std::fs::read(&path) {
            Ok(body) => match serde_json::from_slice::<WorkflowProposal>(&body) {
                Ok(proposal) => Some(proposal),
                Err(err) => {
                    tracing::warn!(path = %path.display(), "skipping unreadable proposal: {err}");
                    None
                }
            },
            Err(err) => {
                tracing::warn!(path = %path.display(), "skipping unreadable proposal: {err}");
                None
            }
        })
        .filter(|proposal| proposal.workflow_id == workflow_id)
        .collect();
    // Ids lead with a zero-padded stamp, so this is chronological.
    proposals.sort_by(|a, b| b.id.cmp(&a.id));
    Ok(proposals)
}

#[cfg(test)]
#[path = "proposals_tests.rs"]
mod tests;
