//! A graph change an agent suggests but does not make.
//!
//! The whole point of the type. An evolution pass reads a workflow's history
//! and often concludes something should change — but a model that edits a saved
//! graph on its own reasoning is a model that can quietly break a workflow
//! nobody was watching. So a pass produces a *proposal*: a checked, dry-run
//! patch that sits on disk until an operator accepts it.
//!
//! Proposals are host state like runs and notes, not part of the versioned
//! document, because most of them never become one.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::note::NoteId;
use super::run::RunId;
use super::workflow::WorkflowId;
use crate::diagnostics::Diagnosis;

/// A proposal's identifier.
pub type ProposalId = String;

/// Where a proposal stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalStatus {
    /// Waiting on an operator.
    Pending,
    /// Applied to the saved graph.
    Accepted,
    /// Turned down. The reason becomes a note, so a later pass does not propose
    /// it again.
    Rejected,
    /// The graph moved on before anyone decided.
    ///
    /// Kept as its own state rather than folded into "rejected": nobody
    /// disagreed with this proposal, it simply cannot be applied to a graph it
    /// was not computed against.
    Stale,
}

/// What checking a proposal found.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProposalVerification {
    /// Whether the proposal applies cleanly and simulates without new problems.
    pub ok: bool,
    /// Epoch-millisecond stamp of the check.
    pub verified_at: u64,
    /// Why it did not pass: op errors, engine validation, or gate failures.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<String>,
    /// What a dry run of the patched graph reported. Absent when the patch
    /// could not be applied far enough to simulate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnosis: Option<Diagnosis>,
}

/// A change to a workflow, argued for and checked but not made.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowProposal {
    /// This proposal's id.
    pub id: ProposalId,
    /// The workflow it would change.
    pub workflow_id: WorkflowId,
    /// Epoch-millisecond stamp of when it was made.
    pub created_at: u64,
    /// The argument for it, in the proposer's own words. What an operator reads
    /// before deciding.
    pub rationale: String,
    /// The engine's patch language, as raw JSON.
    ///
    /// Deliberately not a typed `Vec<GraphOp>`: the ops surface only ever
    /// *deserializes* that type, and keeping the proposal as the JSON it
    /// arrived as means a stored proposal stays readable across engine
    /// versions rather than becoming unloadable when the op enum changes.
    pub ops: Value,
    /// The runs that motivated it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence_runs: Vec<RunId>,
    /// The notes it was reasoned from.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub note_ids: Vec<NoteId>,
    /// A fingerprint of the graph these ops were computed against.
    ///
    /// Checked again at accept time. Ops are positional edits to a specific
    /// graph, so applying them to one that has since changed is not a merge —
    /// it is a silent, arbitrary rewrite. A mismatch makes the proposal
    /// [`ProposalStatus::Stale`] rather than applying it anyway.
    pub base_fingerprint: String,
    /// What checking it found. `None` before it has been checked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification: Option<ProposalVerification>,
    /// Where it stands.
    pub status: ProposalStatus,
    /// Epoch-millisecond stamp of the decision, when one was made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_at: Option<u64>,
    /// Why it was turned down, when it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_reason: Option<String>,
}

impl WorkflowProposal {
    /// Whether this proposal is still awaiting a decision.
    pub fn is_pending(&self) -> bool {
        self.status == ProposalStatus::Pending
    }

    /// Whether an operator could apply this proposal as it stands.
    ///
    /// Both halves matter: a proposal that failed verification is kept on disk
    /// as evidence for the next pass, but it is not something to offer.
    pub fn is_applicable(&self) -> bool {
        self.is_pending() && self.verification.as_ref().is_some_and(|check| check.ok)
    }
}

/// Fingerprint a graph, for detecting that it moved under a proposal.
///
/// SHA-256 of the graph's canonical JSON. Serialization is stable for a given
/// engine version, which is all this needs: it is a same-process, same-build
/// equality check, not a durable content address.
pub fn fingerprint(graph: &crate::model::WorkflowGraph) -> String {
    use sha2::{Digest, Sha256};
    match serde_json::to_vec(graph) {
        Ok(canonical) => format!("{:x}", Sha256::digest(&canonical)),
        // A graph that fails to serialize (a non-finite `Position`, for
        // instance) must not fingerprint the same as every other graph that
        // also fails to serialize. Hashing empty bytes would do exactly that,
        // letting two genuinely different broken graphs compare equal and a
        // stale proposal pass a freshness check it should fail. A fresh random
        // token can never equal a caller-supplied `expected_fingerprint`, so
        // the comparison this backs always reports "changed" instead.
        Err(_) => format!("unfingerprintable:{}", crate::ids::token()),
    }
}
