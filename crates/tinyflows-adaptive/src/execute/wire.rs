//! The contract between the loop and whatever runs the graph.
//!
//! The loop decides; an engine executes. Those two may sit in one process or on
//! opposite ends of a socket, and nothing above this module should be able to
//! tell which. This is the shape that crosses when they are apart — and,
//! deliberately, the shape used when they are together too.
//!
//! # Steps, not the final output
//!
//! A run's [`RunOutcome::output`] is a per-node map and looks like the obvious
//! thing to send. It is a lossy projection of the steps, and lossy in the four
//! places that matter to triage:
//!
//! * no `status`, so a node whose error an `on_error` policy swallowed is
//!   indistinguishable from one that worked — that message survives *only* on
//!   the step;
//! * no duration;
//! * no null-binding diagnostics;
//! * a looped node collapses to one entry however many times it ran;
//! * and a run that returned `Err` has **no output at all**, while its steps are
//!   all still there. That is the run most in need of triage.
//!
//! So the steps cross, and the server reconstructs the rest. [`Diagnosis`](tinyflows::diagnostics::Diagnosis) is
//! not sent either: `diagnose` is a pure function of the graph and the steps,
//! the server already has the graph, and re-deriving it there is both smaller
//! and impossible to disagree about.
//!
//! # Two budgets, applied per node
//!
//! [`bounded_within`] is **whole-value and non-recursive**: hand it a map of
//! twelve nodes where one returned 300 KB and it replaces the entire map with a
//! truncated preview of the serialized string. Every other node's output is
//! gone — not trimmed, gone.
//!
//! So bounding happens **per node**, never on the aggregate, at two budgets:
//!
//! * [`RECORD_BUDGET`] on [`StepRecord::output`] — the durable record, written
//!   once, generous.
//! * [`PROMPT_BUDGET`] on the reconstructed [`RunOutcome::output`] — what the
//!   judge reads, where a dozen node outputs share one context window.
//!
//! Both come from the engine's own note on the function: a durable record uses
//! a generous budget because it is written once; a projection for a model uses
//! a much smaller one.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tinyflows::engine::RunOutcome;
use tinyflows::evidence::bounded_within;
use tinyflows::expr::NullResolution;
use tinyflows::model::WorkflowGraph;
use tinyflows::observability::{ExecutionStep, StepStatus};
use tinyflows::transcript::TranscriptEntry;

use super::Ran;

/// Per-node budget for the durable record. Written once; generous.
pub const RECORD_BUDGET: usize = 256 * 1024;

/// Aggregate budget for one step's transcript, in bytes of entry text.
///
/// A per-entry bound is not enough on its own. `TranscriptEntry::bounded` caps
/// one entry at 4 KiB, and a `per_item` node folds every item's turn into ONE
/// step — so a few thousand entries reach the 16 MB limit a Mongo document
/// may hold, in exactly the production-only way the ledger's own note about
/// one-document-per-step warns of. Worse, `save_steps` deletes before it
/// upserts, so an oversized write destroys the previous record and then fails.
pub const TRANSCRIPT_BUDGET: usize = RECORD_BUDGET;

/// Kept from each end when a transcript is over budget.
const TRANSCRIPT_EDGE: usize = 32;

/// Bytes of one entry's `kind` kept.
///
/// A kind is a discriminator — `tool_call`, `agent_thinking` — so this is
/// generous for anything meant. `TranscriptEntry::bounded` caps `text` and
/// not this, and the field is public, so without a cap here four entries
/// carrying their payload in `kind` walk straight past the budget.
const TRANSCRIPT_KIND_BYTES: usize = 128;

/// What one entry costs against [`TRANSCRIPT_BUDGET`].
///
/// `kind` counts as well as `text`: it is host-supplied and an open set, so a
/// budget that ignored it could be walked past by a harness that puts its
/// payload there.
fn entry_cost(entry: &TranscriptEntry) -> usize {
    entry.kind.len() + entry.text.len()
}

/// Trims `entries` to [`TRANSCRIPT_BUDGET`], keeping both ends.
///
/// Two passes, because there are two ways to be over budget and each needs its
/// own answer:
///
/// 1. **Every entry is re-bounded.** `TranscriptEntry::bounded` is the
///    per-entry cap, but nothing forces a harness to build its entries through
///    it — the struct's fields are public. So one entry larger than a whole
///    Mongo document can arrive, and no count-based rule would catch it.
/// 2. **Then the middle is dropped** until the aggregate fits. Head *and*
///    tail, with a marker between: the start says how the agent approached the
///    work and the end says how it concluded, and the middle is the most
///    droppable part of a long tool loop. Clipping only the tail would lose the
///    conclusion, which is usually why someone opened the transcript.
///
/// There is deliberately no early return on a short transcript. Sixty-four
/// entries can be over budget just as four thousand can, and an early return
/// keyed on count was exactly the hole review found in the first version of
/// this function.
fn bounded_transcript(entries: &[TranscriptEntry]) -> Vec<TranscriptEntry> {
    let out: Vec<TranscriptEntry> = entries
        .iter()
        .map(|e| {
            let mut kind = e.kind.clone();
            if kind.len() > TRANSCRIPT_KIND_BYTES {
                let end = kind
                    .char_indices()
                    .map(|(index, _)| index)
                    .take_while(|index| *index <= TRANSCRIPT_KIND_BYTES)
                    .last()
                    .unwrap_or(0);
                kind.truncate(end);
            }
            TranscriptEntry::bounded(e.at_ms, kind, e.text.clone())
        })
        .collect();

    let total: usize = out.iter().map(entry_cost).sum();
    if total <= TRANSCRIPT_BUDGET {
        return out;
    }

    // One pass from each end, never a rescan. Walking the vector and removing
    // from its middle re-measures the whole thing per iteration and shifts the
    // suffix each time — quadratic, on work that happens after the agent has
    // finished and while a report is waiting to go out.
    //
    // Half the budget from each end, so a transcript that is huge at one end
    // cannot starve the other.
    let half = TRANSCRIPT_BUDGET / 2;

    let mut head = 0usize;
    let mut spent = 0usize;
    while head < out.len() && head < TRANSCRIPT_EDGE {
        let cost = entry_cost(&out[head]);
        if spent + cost > half {
            break;
        }
        spent += cost;
        head += 1;
    }

    let mut tail = 0usize;
    spent = 0;
    while tail < out.len() - head && tail < TRANSCRIPT_EDGE {
        let cost = entry_cost(&out[out.len() - 1 - tail]);
        if spent + cost > half {
            break;
        }
        spent += cost;
        tail += 1;
    }

    let dropped = out.len() - head - tail;
    if dropped == 0 {
        return out;
    }

    let at_ms = out.get(head).map_or(0, |e| e.at_ms);
    let mut trimmed: Vec<TranscriptEntry> = Vec::with_capacity(head + tail + 1);
    trimmed.extend_from_slice(&out[..head]);
    trimmed.push(TranscriptEntry::bounded(
        at_ms,
        "error",
        format!("…[{dropped} transcript entries elided to fit the record budget]"),
    ));
    trimmed.extend_from_slice(&out[out.len() - tail..]);
    trimmed
}

/// Per-node budget for what the judge reads. A dozen of these share one context
/// window, so it is much smaller than the record.
pub const PROMPT_BUDGET: usize = 4 * 1024;

/// Whether a node succeeded.
///
/// A mirror of [`StepStatus`], which does not derive `Serialize`. Mirrored
/// rather than patched upstream so the wire format can version independently of
/// the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepOutcome {
    /// The node executed and produced output.
    Success,
    /// The node's executor errored, after any retries.
    Error,
}

/// One node activation, as it crosses the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StepRecord {
    /// The node that ran. Not unique across the list: a looped node appears
    /// once per iteration, in order, which is the history `output` loses.
    pub node_id: String,
    /// Whether it succeeded. The only place a swallowed error is visible.
    pub status: StepOutcome,
    /// What it emitted, bounded to the budget it was recorded at.
    pub output: Value,
    /// Wall-clock milliseconds. `u64` rather than the engine's `u128`, which
    /// has no faithful JSON representation; saturating, because a node that ran
    /// for 584 million years has a different problem.
    pub duration_ms: u64,
    /// Config expressions that resolved to null during this activation.
    #[serde(default)]
    pub null_bindings: Vec<NullResolution>,
    /// What the harness did inside the node, in order.
    ///
    /// Carried rather than dropped because [`Ran::steps`](crate::execute::Ran)
    /// is the archival record — "every node activation, at full record
    /// fidelity" — and for an `agent` node the transcript is most of what there
    /// is to know.
    ///
    /// Bounded differently from `output`, not left unbounded: individual
    /// entries are already capped, so what matters here is the *aggregate*
    /// (see [`TRANSCRIPT_BUDGET`]), and an over-budget transcript keeps both
    /// ends rather than being clipped from one — a truncated payload loses its
    /// tail, a truncated transcript would lose its conclusion.
    ///
    /// Empty for every non-`agent` node and for a harness that reports none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub transcript: Vec<TranscriptEntry>,
}

impl StepRecord {
    /// Record a step, bounding its output to `budget`.
    #[must_use]
    pub fn bounded(step: &ExecutionStep, budget: usize) -> Self {
        Self {
            node_id: step.node_id.clone(),
            status: match step.status {
                StepStatus::Success => StepOutcome::Success,
                StepStatus::Error => StepOutcome::Error,
            },
            output: bounded_within(&step.output, budget),
            duration_ms: u64::try_from(step.duration_ms).unwrap_or(u64::MAX),
            null_bindings: step.diagnostics.clone(),
            transcript: bounded_transcript(&step.transcript),
        }
    }

    /// Back to an engine step, so `diagnose` can read it on the far side.
    #[must_use]
    pub fn to_step(&self) -> ExecutionStep {
        ExecutionStep {
            node_id: self.node_id.clone(),
            status: match self.status {
                StepOutcome::Success => StepStatus::Success,
                StepOutcome::Error => StepStatus::Error,
            },
            output: self.output.clone(),
            duration_ms: u128::from(self.duration_ms),
            diagnostics: self.null_bindings.clone(),
            transcript: self.transcript.clone(),
        }
    }
}

/// What the loop asks an engine to run.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunRequest {
    /// Correlates the reply. The loop's own attempt identity, not a task id.
    pub attempt_id: String,
    /// The graph to run. Validated by intake before it ever gets here.
    pub graph: WorkflowGraph,
    /// Values for the graph's declared inputs.
    pub inputs: Map<String, Value>,
}

/// What comes back.
///
/// Everything the closing layer reads, and nothing else: no history, no
/// workflow, no lessons. A device cannot see the episode it is part of.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunReport {
    /// Echoed from the request.
    pub attempt_id: String,
    /// Every node activation, in completion order.
    pub steps: Vec<StepRecord>,
    /// Gates the run parked on.
    #[serde(default)]
    pub pending_approvals: Vec<String>,
    /// Whether it wound down on a cancellation.
    #[serde(default)]
    pub cancelled: bool,
    /// What the host says changed outside the run.
    #[serde(default)]
    pub changed: String,
    /// The engine error, when the run did not complete.
    #[serde(default)]
    pub failed: Option<String>,
    /// What it cost, in the host's unit. Zero means not measured.
    ///
    /// Carried from the start even though nothing consumes it yet: the runner
    /// is the only thing that knows the number, and a column added later cannot
    /// distinguish a genuine zero from a retrofitted one.
    #[serde(default)]
    pub cost_usd: f64,
}

impl RunReport {
    /// Rebuild what the closing layer takes.
    ///
    /// `graph` comes from the loop's own side — it authored or selected it — so
    /// nothing here trusts the runner for the shape of the thing it ran.
    ///
    /// The reconstructed [`RunOutcome::output`] is bounded at
    /// [`PROMPT_BUDGET`], not [`RECORD_BUDGET`]: it exists to be rendered into
    /// the judge's prompt. The full-fidelity per-node record stays on
    /// [`Ran::steps`].
    #[must_use]
    pub fn into_ran(self, graph: &WorkflowGraph) -> Ran {
        let steps: Vec<ExecutionStep> = self.steps.iter().map(StepRecord::to_step).collect();
        let diagnosis = tinyflows::diagnostics::diagnose(graph, &steps);

        // Last activation wins, matching the engine's own final state: a looped
        // node's latest output is what a downstream binding would have read.
        // The per-iteration history is not lost — it is on `steps`.
        let mut nodes = Map::new();
        for step in &self.steps {
            nodes.insert(
                step.node_id.clone(),
                bounded_within(&step.output, PROMPT_BUDGET),
            );
        }

        let mut output = Map::new();
        if !nodes.is_empty() {
            output.insert("nodes".into(), Value::Object(nodes));
        }
        if let Some(message) = &self.failed {
            output.insert("error".into(), json!(message));
        }

        Ran {
            // A wire report says what happened, not where to pick it back up:
            // the boundary lives in the host's checkpointer, and only a host
            // that keeps one can name it.
            resume: None,
            outcome: RunOutcome {
                output: Value::Object(output),
                pending_approvals: self.pending_approvals,
                cancelled: self.cancelled,
            },
            diagnosis,
            changed: self.changed,
            failed: self.failed,
            steps: self.steps,
            cost_usd: self.cost_usd,
        }
    }
}

#[cfg(test)]
#[path = "wire_tests.rs"]
mod tests;
