//! The data model for stored workflows, their runs, and what a host learns
//! about them.
//!
//! A *workflow* is a [`crate::model::WorkflowGraph`] — the engine's own
//! portable JSON shape — plus the bookkeeping a host needs to find it, list
//! it, and say where it came from. The graph itself is deliberately not
//! re-modelled here: it is the contract shared with the engine and with the
//! sibling hosts that embed it, and a parallel host-side copy would only
//! drift.
//!
//! Runs are recorded rather than merely streamed, so a workflow that paused for
//! approval or died with the process can be found again by id.
//!
//! The submodules split the model by lifetime rather than by shape, because
//! that is what decides where each type is stored:
//!
//! - [`workflow`] — the versioned document an operator edits.
//! - [`run`] — one execution's durable record, written once and never revised.
//! - [`note`] — what the host has learned about a workflow across runs.
//! - [`proposal`] — a graph change suggested but not yet made.
//! - [`error`] — the failure vocabulary every surface reports through.
//! - [`TranscriptEntry`] — one line of what an agent did inside a step,
//!   re-exported from [`crate::transcript`] because the engine carries it too.
//!
//! Why a failed run failed, in terms an author can act on, is
//! [`crate::diagnostics`] — reading a run's steps is a pure function of the
//! engine's own records, so it is not gated behind this feature and is
//! re-exported here only for the callers that always reached it through
//! `store::types`.

mod error;
mod note;
mod proposal;
mod run;
mod workflow;

#[cfg(test)]
#[path = "types_tests.rs"]
mod tests;

pub use crate::diagnostics::Diagnosis;
pub use error::WorkflowError;
pub use note::{NoteId, NoteKind, NoteSource, WorkflowNote};
pub use proposal::{
    ProposalId, ProposalStatus, ProposalVerification, WorkflowProposal, fingerprint,
};

pub use run::{
    LEGACY_TRUNCATED_KEY, RunExecutor, RunId, RunOrigin, RunRecord, RunStatus, RunStep,
    TRUNCATED_KEY, bounded_evidence, bounded_within, is_truncated,
};
// Re-exported, not owned: `TranscriptEntry` is engine surface (it rides an
// `ExecutionStep` and an `AgentRunOutcome`), so it lives at `crate::transcript`
// and cannot sit behind the `store` feature. Kept here so every path that has
// always read `store::types::TranscriptEntry` still resolves.
pub use crate::transcript::TranscriptEntry;
pub use workflow::{
    WorkflowDefaults, WorkflowId, WorkflowRecord, WorkflowRevision, WorkflowSummary,
    record_fingerprint,
};
