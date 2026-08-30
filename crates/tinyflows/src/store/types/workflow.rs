//! The stored workflow document and its listing and history views.
//!
//! These are the versioned half of the model: every write to a
//! [`WorkflowRecord`] snapshots the superseded copy as a [`WorkflowRevision`],
//! which is what makes an edit an operator disagrees with reversible.

use std::path::PathBuf;

use crate::model::{WorkflowGraph, WorkflowInput};
use serde::{Deserialize, Serialize};

/// A workflow's stable identifier: the `id` in its document, defaulting to the
/// filename stem when the document omits one.
pub type WorkflowId = String;

/// A stored workflow: the engine graph plus where this host found it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowRecord {
    /// The workflow's stable id.
    pub id: WorkflowId,
    /// Display name; falls back to the id when the document omits one.
    pub name: String,
    /// Operator-facing description of what the workflow does.
    #[serde(default)]
    pub description: String,
    /// Whether the workflow may be run. A disabled workflow still lists and
    /// validates, so an operator can repair one without it firing.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// What every `agent` node in this workflow runs on unless it says
    /// otherwise.
    #[serde(default, skip_serializing_if = "WorkflowDefaults::is_empty")]
    pub defaults: WorkflowDefaults,
    /// The engine graph.
    pub graph: WorkflowGraph,
    /// The file this record was read from, when it came from disk. `None` for a
    /// graph built in memory (an agent's draft, an import not yet saved).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub source_path: Option<PathBuf>,
}

/// Workflows are enabled unless a document says otherwise.
fn default_enabled() -> bool {
    true
}

/// A workflow's standing choice of harness and model.
///
/// The middle layer between an `agent` node's own `config` and the host's
/// `workflows` config, and the one an author reaches for most: "this whole plan
/// runs on Codex" is a property of the plan, not of every node in it and not of
/// the machine that happens to run it.
///
/// Stored as free-form strings rather than parsed types because a workflow may
/// legitimately name a custom harness preset only some hosts expose. The
/// meaning of these strings — and the refusal of one that cannot be a harness —
/// lives in [`crate::flow_engine::harness_choice`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowDefaults {
    /// The harness every `agent` node runs on unless it names its own: a
    /// built-in CLI (`claude`, `codex`, `opencode`) or a custom preset id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    /// The model hint sent with every dispatch this workflow makes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl WorkflowDefaults {
    /// Whether this workflow states no preference at all.
    ///
    /// Kept so an unset block is omitted from the document entirely: a file an
    /// operator opens should not grow two null fields per workflow to say
    /// nothing.
    pub fn is_empty(&self) -> bool {
        self.harness.is_none() && self.model.is_none()
    }
}

impl WorkflowRecord {
    /// The listing view of this record.
    pub fn summary(&self) -> WorkflowSummary {
        WorkflowSummary {
            id: self.id.clone(),
            name: self.name.clone(),
            description: self.description.clone(),
            enabled: self.enabled,
            node_count: self.graph.nodes.len(),
            trigger_kind: self.trigger_kind(),
            inputs: self.inputs().to_vec(),
        }
    }

    /// The workflow's declared inputs — what a caller must supply to run it.
    ///
    /// Lives on the engine graph, so this is a shorthand rather than a second
    /// copy. Empty for a workflow that takes none.
    pub fn inputs(&self) -> &[WorkflowInput] {
        &self.graph.inputs
    }

    /// The graph's trigger kind, as a lowercase string.
    ///
    /// Read out of the trigger node's free-form config rather than a typed
    /// field, because that is where the engine keeps it. `None` when the graph
    /// has no single trigger — which validation will also report, so this stays
    /// quiet rather than duplicating the error.
    pub fn trigger_kind(&self) -> Option<String> {
        let trigger = self.graph.trigger()?;
        trigger
            .config
            .get("trigger_kind")
            .and_then(|value| value.as_str())
            .map(str::to_string)
    }
}

/// Fingerprint every persisted field in a workflow record.
///
/// The source path is a property of the store read, not part of the workflow
/// document, so it is deliberately excluded. Definition compare-and-swap
/// writes use this fingerprint because a graph-only comparison would miss
/// concurrent changes to defaults or other workflow metadata.
pub fn record_fingerprint(record: &WorkflowRecord) -> String {
    use sha2::{Digest, Sha256};

    let mut persisted = record.clone();
    persisted.source_path = None;
    match serde_json::to_vec(&persisted) {
        Ok(canonical) => format!("{:x}", Sha256::digest(&canonical)),
        // Same reasoning as `proposal::fingerprint`: hashing empty bytes on a
        // serialization failure would let a compare-and-swap write accept a
        // stale record whenever both the expected and current record happen
        // to fail to serialize the same way. A fresh token can never match a
        // caller's `expected_fingerprint`, so the write is refused instead.
        Err(_) => format!("unfingerprintable:{}", crate::ids::token()),
    }
}

/// A workflow reduced to what a list needs — the shape advertised to the
/// orchestrator and rendered in the TUI, so neither has to hold whole graphs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowSummary {
    /// The workflow's stable id.
    pub id: WorkflowId,
    /// Display name.
    pub name: String,
    /// Operator-facing description.
    pub description: String,
    /// Whether the workflow may be run.
    pub enabled: bool,
    /// How many nodes the graph has.
    pub node_count: usize,
    /// The trigger kind, when the graph declares exactly one trigger.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger_kind: Option<String>,
    /// The workflow's declared inputs — what a caller must supply to run it.
    ///
    /// Carried on the *listing* view deliberately: the TUI has to know whether
    /// to prompt before it runs the selected workflow, and the orchestrator has
    /// to know what to collect before it asks. Both would otherwise need a
    /// second fetch of the whole graph just to answer "does this take
    /// arguments?". Omitted from the wire for a workflow that takes none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<WorkflowInput>,
}

/// A copy of a workflow from before it was last written over.
///
/// Kept so an operator can disagree with an edit after the fact. That matters
/// most for the copilot, which writes to the store directly and would otherwise
/// leave a misread instruction as the only surviving version of a graph.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowRevision {
    /// This snapshot's id, unique within its workflow. Sorts chronologically.
    pub id: String,
    /// Epoch-millisecond stamp of when this copy stopped being current.
    ///
    /// When it was *superseded*, not when it was authored — a revision is
    /// named by the edit that replaced it, which is what an operator scanning
    /// history is looking for.
    pub superseded_at: u64,
    /// The workflow as it was.
    pub record: WorkflowRecord,
}
