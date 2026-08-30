//! What a host has learned about a workflow.
//!
//! A workflow could previously say what it *is* and what one run *did*, but
//! nothing carried across runs. Every diagnosis started from zero, so the same
//! cause was re-derived every time it recurred, and a conclusion an operator
//! reached last week was gone by the time it mattered again.
//!
//! A note is one durable claim about a workflow, and the journal is the set of
//! them. Notes are deliberately *not* part of the workflow document: they churn
//! on every failure, and the document is versioned through a twenty-entry
//! revision ring that an operator's real edit history has to fit into.

use serde::{Deserialize, Serialize};

use super::run::RunId;
use super::workflow::WorkflowId;

/// A note's identifier, unique within its workflow's journal.
pub type NoteId = String;

/// What a note claims.
///
/// Separated because they age differently, and the brief that reads them
/// weights them differently: an observation is evidence about one moment, a
/// constraint is a rule the next proposal has to obey.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoteKind {
    /// Something that happened, stated without explanation. The safest kind to
    /// write from automation, because it makes no claim about cause.
    Observation,
    /// A proposed cause, not yet confirmed. Worth recording precisely because a
    /// later run either supports it or does not.
    Hypothesis,
    /// A rule about this workflow that any future change must respect.
    Constraint,
    /// A change that was made and what it was meant to fix.
    Fix,
    /// A change that was considered and turned down, with the reason.
    ///
    /// The kind that makes the loop converge rather than merely terminate:
    /// without it, an idea an operator has already rejected is proposed again
    /// the next time the same evidence turns up.
    Rejection,
}

/// Who wrote a note.
///
/// An agent's claim and an operator's instruction are not the same kind of
/// thing, and a brief that presented them identically would let a model's own
/// guess outweigh what a human actually said.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum NoteSource {
    /// Written by a model during an evolution pass.
    Agent {
        /// The model that wrote it, when known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
    /// Written by a person.
    Operator,
    /// Written by the host itself, from a run record — no model involved.
    ///
    /// The kind that is always safe to write: it needs no dispatch, so it
    /// survives a missing harness, a timed-out turn, and a reply that was pure
    /// prose.
    System,
}

/// One durable claim about a workflow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowNote {
    /// This note's id. Leads with a zero-padded timestamp, so a lexical sort is
    /// a chronological one — the same scheme workflow revisions use.
    pub id: NoteId,
    /// The workflow this note is about.
    pub workflow_id: WorkflowId,
    /// What the note claims.
    pub kind: NoteKind,
    /// The claim itself, in whoever's words wrote it.
    pub text: String,
    /// Epoch-millisecond stamp of when it was written.
    pub recorded_at: u64,
    /// Who wrote it.
    pub source: NoteSource,
    /// The runs this note is evidence from.
    ///
    /// Provenance rather than decoration: a note whose evidence is one flaky
    /// run should not be read the same way as one drawn from five.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub run_ids: Vec<RunId>,
    /// The note that replaced this one, when a later note did.
    ///
    /// Superseded notes stay listed — an operator reading history wants to see
    /// what was believed and when — but are kept out of briefs, so a model is
    /// not asked to reason from a claim already known to be wrong.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<NoteId>,
    /// Whether this note is exempt from pruning.
    ///
    /// An operator's own words are pinned by default: automation writing a
    /// hundred observations must not be able to evict what a person said.
    #[serde(default)]
    pub pinned: bool,
}

impl WorkflowNote {
    /// Whether this note should appear in a brief.
    ///
    /// Superseded notes are history, not context.
    pub fn is_current(&self) -> bool {
        self.superseded_by.is_none()
    }
}
