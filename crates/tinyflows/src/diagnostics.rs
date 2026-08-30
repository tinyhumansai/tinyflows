//! Reading a simulation's steps for the failures a green run hides.
//!
//! A dry run that "passes" proves less than it looks like it does. Every node
//! ran, every node returned, the outcome is a JSON object — and a step whose
//! only input expression resolved to `null` looks exactly the same as one that
//! got what it needed. Null is a legal value: the engine has no complaint, the
//! run record is all green, and the workflow does nothing when it runs for real.
//!
//! So the point of a dry run at authoring time is not the outcome. It is the
//! *steps*, and specifically four things in them that the outcome cannot say:
//!
//! - a binding that resolved to null,
//! - an `agent` node that would dispatch a harness session with an empty prompt,
//! - a node that errored but whose `on_error` policy swallowed it,
//! - a node that never ran at all because a condition routed the sample past it.
//!
//! The last two are the ones a naive reading misses. An error hidden by
//! `on_error: continue` leaves a step marked failed with *empty* diagnostics, so
//! a check that only looked at diagnostics sees nothing; and a node that never
//! executed produces no step at all, so a check that only walked the steps it
//! got would report a clean run on a graph where half the work was skipped.
//!
//! # What this cannot see
//!
//! The engine traces null-resolved expressions for `agent`, `tool_call`, and
//! `http_request` nodes only — the kinds that hand a resolved config to
//! something outside itself. A `transform` or `condition` whose expression
//! resolves to null emits no diagnostic, so a transform that quietly sets a
//! field to null passes here. That is a real gap and the reason the *gates*
//! ([`crate::gates`]) exist alongside this: they read the graph statically and
//! catch the shapes that are wrong before anything runs.
//!
//! Adapted from a sibling host's dry-run diagnostics.

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use crate::model::{NodeKind, WorkflowGraph};
use crate::observability::{ExecutionStep, Run, RunObserver, StepStatus};
use serde::{Deserialize, Serialize};

/// Collects every step a run reports, for reading once it settles.
///
/// The whole of the observer a simulation needs: no events, no progress, no
/// plan — just the record.
#[derive(Default)]
pub struct CapturingObserver {
    steps: Mutex<Vec<ExecutionStep>>,
}

impl CapturingObserver {
    /// The steps captured so far, in completion order.
    pub fn steps(&self) -> Vec<ExecutionStep> {
        self.steps.lock().expect("steps lock").clone()
    }
}

impl RunObserver for CapturingObserver {
    fn on_step_finish(&self, step: &ExecutionStep) {
        self.steps.lock().expect("steps lock").push(step.clone());
    }

    fn on_run_finish(&self, _run: &Run) {}
}

/// One expression that resolved to null during the simulation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NullBinding {
    /// The node whose config held it.
    pub node_id: String,
    /// The dotted config location — `args.to`, `args.cc.0`.
    pub location: String,
    /// The expression as written.
    pub expression: String,
    /// Whether a dry run can actually settle this.
    ///
    /// The honest half of the diagnostic. When a binding reads from an upstream
    /// node the sandbox cannot faithfully stand in for — an `agent` node, whose
    /// real output is whatever a harness replies — a null here is what the
    /// *mock* produced, not proof the wiring is wrong. Saying so is what stops
    /// an agent rewiring a correct graph over and over against a check that was
    /// never going to go green.
    pub unverifiable: bool,
    /// The upstream node the binding reads from, when it reads from one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reads_from: Option<String>,
    /// What to do about it.
    pub suggestion: String,
}

/// A node that errored where the graph's own error policy hid it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HiddenError {
    /// The node that failed.
    pub node_id: String,
    /// What it reported, if anything readable came back.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// A node the simulation never reached.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NeverRan {
    /// The node that did not execute.
    pub node_id: String,
    /// The condition upstream of it that routed the run elsewhere, if one was
    /// found.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub routed_by: Option<String>,
}

/// What a simulation says about a graph, beyond whether it completed.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Diagnosis {
    /// Bindings that resolved to null.
    pub null_bindings: Vec<NullBinding>,
    /// Agent nodes that would run with an empty instruction.
    pub empty_prompts: Vec<String>,
    /// Failures the graph's error policy swallowed.
    pub hidden_errors: Vec<HiddenError>,
    /// Nodes the sample never reached. A warning, not a failure.
    pub never_ran: Vec<NeverRan>,
}

impl Diagnosis {
    /// Whether anything here should stop an author calling the graph done.
    ///
    /// [`never_ran`](Self::never_ran) is deliberately excluded: a condition
    /// routing one sample down one branch is what a condition is *for*, and
    /// failing on it would make every branching graph unbuildable.
    pub fn is_clean(&self) -> bool {
        self.null_bindings.iter().all(|b| b.unverifiable)
            && self.empty_prompts.is_empty()
            && self.hidden_errors.is_empty()
    }
}

/// Read `steps` against `graph` for what the outcome cannot say.
pub fn diagnose(graph: &WorkflowGraph, steps: &[ExecutionStep]) -> Diagnosis {
    let mut diagnosis = Diagnosis::default();

    for step in steps {
        let node = graph.nodes.iter().find(|n| n.id == step.node_id);

        for null in &step.diagnostics {
            // An agent node whose *instruction* resolved to null is its own
            // class: the node still runs, and dispatches a whole harness
            // session with nothing to do.
            let is_prompt = matches!(node.map(|n| &n.kind), Some(NodeKind::Agent))
                && matches!(null.location.as_str(), "prompt" | "instruction");
            if is_prompt {
                diagnosis.empty_prompts.push(step.node_id.clone());
                continue;
            }
            diagnosis
                .null_bindings
                .push(null_binding(graph, &step.node_id, null));
        }

        // An error hidden by `on_error: continue|route` carries no diagnostics
        // at all, so it has to be read off the step's status and its output —
        // which is the only place the message survives.
        if matches!(step.status, StepStatus::Error) {
            diagnosis.hidden_errors.push(HiddenError {
                node_id: step.node_id.clone(),
                message: error_message(&step.output),
            });
        }
    }

    let ran: HashSet<&str> = steps.iter().map(|s| s.node_id.as_str()).collect();
    for node in &graph.nodes {
        // Only the kinds that do outside work are worth reporting. A transform
        // that was routed past is not a surprise worth a warning.
        if !matches!(
            node.kind,
            NodeKind::Agent | NodeKind::ToolCall | NodeKind::HttpRequest
        ) {
            continue;
        }
        if ran.contains(node.id.as_str()) {
            continue;
        }
        diagnosis.never_ran.push(NeverRan {
            node_id: node.id.clone(),
            routed_by: upstream_condition(graph, &node.id),
        });
    }

    diagnosis
}

/// Build one null-binding entry, deciding whether a dry run could settle it.
fn null_binding(
    graph: &WorkflowGraph,
    node_id: &str,
    null: &crate::expr::NullResolution,
) -> NullBinding {
    let reads_from = crate::bindings::parse_node_binding(&null.expression)
        .map(|binding| binding.node_id)
        .filter(|id| graph.nodes.iter().any(|n| &n.id == id));

    // A binding onto an `agent` node cannot be checked here. The sandbox stands
    // in for a harness with a canned reply, so a null says the *mock* had no
    // such field — not that a real session would not produce one.
    let unverifiable = reads_from
        .as_deref()
        .and_then(|id| graph.nodes.iter().find(|n| n.id == id))
        .is_some_and(|node| node.kind == NodeKind::Agent);

    let suggestion = if unverifiable {
        format!(
            "reads from agent node `{}`, whose real output is whatever the harness replies — a \
             dry run cannot confirm this field exists. Check it against what you asked that node \
             to produce rather than re-wiring against the sandbox.",
            reads_from.clone().unwrap_or_default()
        )
    } else {
        "resolved to null, so this step ran with an empty value. Check the path against the \
         upstream node's actual output shape; `workflow_catalog` describes each kind's."
            .to_string()
    };

    NullBinding {
        node_id: node_id.to_string(),
        location: null.location.clone(),
        expression: null.expression.clone(),
        unverifiable,
        reads_from,
        suggestion,
    }
}

/// The nearest `condition` upstream of `node_id`, walking edges backwards.
///
/// Named so the warning can say *why* a node was skipped. "`notify` never ran"
/// sends an author looking at `notify`; "`notify` never ran — `check` routed
/// past it" sends them to the node that actually decided.
fn upstream_condition(graph: &WorkflowGraph, node_id: &str) -> Option<String> {
    let mut seen: HashSet<&str> = HashSet::from([node_id]);
    let mut queue: VecDeque<&str> = VecDeque::from([node_id]);

    while let Some(current) = queue.pop_front() {
        for edge in &graph.edges {
            if edge.to_node != current {
                continue;
            }
            let from = edge.from_node.as_str();
            if !seen.insert(from) {
                continue;
            }
            if graph
                .nodes
                .iter()
                .any(|n| n.id == from && n.kind == NodeKind::Condition)
            {
                return Some(from.to_string());
            }
            queue.push_back(from);
        }
    }
    None
}

/// The message an errored step left in its output, if it left a readable one.
fn error_message(output: &serde_json::Value) -> Option<String> {
    output
        .get("error")
        .and_then(|e| {
            e.as_str()
                .map(str::to_string)
                .or_else(|| Some(e.to_string()))
        })
        .filter(|message| !message.trim().is_empty())
}

/// A [`CapturingObserver`] as the engine's observer handle.
pub fn capturing() -> (Arc<CapturingObserver>, Arc<dyn RunObserver>) {
    let observer = Arc::new(CapturingObserver::default());
    (observer.clone(), observer as Arc<dyn RunObserver>)
}

/// What one simulation produced.
///
/// The output and the diagnosis together, because either alone misleads: the
/// output of a graph that did nothing looks like the output of one that worked,
/// and a diagnosis with nothing to show is only meaningful next to a run that
/// actually completed.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DryRun {
    /// The final run state: every node's output, keyed by node id.
    pub output: serde_json::Value,
    /// What the steps said on the way.
    pub diagnosis: Diagnosis,
}

#[cfg(test)]
#[path = "diagnostics_tests.rs"]
mod tests;
