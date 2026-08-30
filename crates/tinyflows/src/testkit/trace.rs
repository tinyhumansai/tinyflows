//! A structured record of what a run actually did.
//!
//! [`RunObserver`](crate::observability::RunObserver) reports a node's *output*
//! after the fact. That is the wrong half of the question. When a workflow runs
//! green and does nothing, the output is exactly what a correct run's output
//! looks like — an object, no errors — and the thing that went wrong is
//! upstream of it: a binding that read from a field no node produces, and so
//! resolved to `null`, which is a legal value the engine has no complaint
//! about.
//!
//! So a trace records what a node was **about to receive**, not only what it
//! returned: every `=`-binding, the value it resolved to, and — when it
//! resolved to nothing — the upstream node it was reading from. That last part
//! is what turns "it produced null" into a pointer at the node that should have
//! produced it.
//!
//! Assembled from the [`StepInterceptor`] (which sees a node before it runs)
//! and a [`RunObserver`](crate::observability::RunObserver) (which sees how long
//! it took), because neither can supply the whole picture alone.

use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::caps::Capabilities;
use crate::data::Item;
use crate::diagnostics::{Diagnosis, diagnose};
use crate::interception::{StepAction, StepFrame, StepInterceptor, StepPhase};
use crate::model::{Node, NodeKind, WorkflowGraph};
use crate::observability::{ExecutionStep, RunObserver};

use super::mocks::{CapCall, MockCaps};

/// One `=`-binding in a node's config, and what it actually resolved to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BindingTrace {
    /// Where in the config it was written — `args.to`, `args.cc.0`.
    pub location: String,
    /// The expression as the author wrote it, `=` and all.
    pub expression: String,
    /// What it resolved to for this activation.
    pub value: Value,
    /// Whether it resolved to nothing.
    ///
    /// Not an error, which is the whole problem: the node ran, the engine was
    /// satisfied, and the field was empty.
    pub is_null: bool,
    /// The upstream node the expression reads from, when it reads from one.
    ///
    /// The pointer. A null binding with `reads_from: Some("fetch_user")` says
    /// where to look; the same null without it says only that something is
    /// wrong.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reads_from: Option<String>,
}

/// Whether a traced activation succeeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceStatus {
    /// The node produced output.
    Success,
    /// The node failed after exhausting its attempts.
    Error,
}

/// One node activation, as the trace records it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TraceStep {
    /// Position in the run's activation sequence, from 0.
    pub seq: u64,
    /// The node that ran.
    pub node_id: String,
    /// Its kind.
    pub node_kind: NodeKind,
    /// The super-step that drove it. A node inside a loop appears once per
    /// pass, distinguished by this.
    pub superstep: usize,
    /// The parallel lane this activation belonged to, when a `scatter` opened
    /// one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lane: Option<String>,
    /// How many execution attempts it consumed. Above 1 means it retried.
    pub attempts: u32,
    /// What the node received.
    pub input: Vec<Item>,
    /// What it emitted. Empty on failure.
    pub output: Vec<Item>,
    /// The port it emitted on, when not the default `main`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<String>,
    /// Whether it succeeded.
    pub status: TraceStatus,
    /// Why it failed, when it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u128,
    /// Every `=`-binding in the node's config, with the value it resolved to.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bindings: Vec<BindingTrace>,
}

impl TraceStep {
    /// The bindings that resolved to nothing.
    ///
    /// The single most useful question to ask of a step that "worked".
    #[must_use]
    pub fn null_bindings(&self) -> Vec<&BindingTrace> {
        self.bindings.iter().filter(|b| b.is_null).collect()
    }
}

/// Everything a traced run recorded.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunTrace {
    /// Each node activation, in the order they settled.
    pub steps: Vec<TraceStep>,
    /// Every capability call the run made, in one sequence.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub calls: Vec<CapCall>,
    /// What the steps say that the outcome cannot — null bindings, empty agent
    /// prompts, errors an `on_error` policy swallowed, and nodes never reached.
    pub diagnosis: Diagnosis,
}

impl RunTrace {
    /// Every activation of `node_id`, in order.
    ///
    /// A node inside a loop or a scatter has more than one.
    #[must_use]
    pub fn steps_for(&self, node_id: &str) -> Vec<&TraceStep> {
        self.steps.iter().filter(|s| s.node_id == node_id).collect()
    }

    /// Whether `node_id` ran at all.
    ///
    /// Worth asking explicitly: a node a condition routed past leaves no step,
    /// and a graph half of which never executed still reports a clean outcome.
    #[must_use]
    pub fn ran(&self, node_id: &str) -> bool {
        self.steps.iter().any(|s| s.node_id == node_id)
    }

    /// The capability calls `node_id` made.
    #[must_use]
    pub fn calls_from(&self, node_id: &str) -> Vec<&CapCall> {
        self.calls
            .iter()
            .filter(|c| c.node_id.as_deref() == Some(node_id))
            .collect()
    }

    /// Every null binding across the run, as `(node id, binding)`.
    #[must_use]
    pub fn null_bindings(&self) -> Vec<(&str, &BindingTrace)> {
        self.steps
            .iter()
            .flat_map(|step| {
                step.null_bindings()
                    .into_iter()
                    .map(move |b| (step.node_id.as_str(), b))
            })
            .collect()
    }

    /// The nodes that failed, in the order they did.
    #[must_use]
    pub fn failed(&self) -> Vec<&TraceStep> {
        self.steps
            .iter()
            .filter(|s| s.status == TraceStatus::Error)
            .collect()
    }

    /// A short human-readable summary, for a log line or a tool reply.
    #[must_use]
    pub fn summary(&self) -> String {
        let nulls = self.null_bindings().len();
        let failed = self.failed().len();
        format!(
            "{} steps, {} capability calls, {failed} failed, {nulls} null bindings",
            self.steps.len(),
            self.calls.len()
        )
    }
}

/// Reads every `=`-binding in `node`'s config against the scope it will be
/// evaluated with.
///
/// Pure: nothing executes. This is the same resolution the node itself performs,
/// done once more so the result can be reported rather than only used.
fn trace_bindings(node: &Node, graph: Option<&WorkflowGraph>, scope: &Value) -> Vec<BindingTrace> {
    crate::bindings::collect_expressions(&node.config)
        .into_iter()
        .map(|(location, expression)| {
            let value = crate::expr::resolve(&json!(expression), scope);
            // Only worth naming an upstream node that actually exists: a typo'd
            // node id in a binding is a different (and louder) problem than a
            // real node whose output lacks the field.
            let reads_from = crate::bindings::parse_node_binding(&expression)
                .map(|binding| binding.node_id)
                .filter(|id| graph.is_none_or(|g| g.nodes.iter().any(|n| &n.id == id)));
            BindingTrace {
                location,
                is_null: value.is_null(),
                value,
                expression,
                reads_from,
            }
        })
        .collect()
}

/// A step that has been seen by the interceptor but not yet timed.
#[derive(Debug)]
struct Pending {
    step: TraceStep,
}

#[derive(Debug, Default)]
struct TraceState {
    /// Completed steps, in settle order.
    steps: Vec<TraceStep>,
    /// Steps awaiting their duration from the observer, oldest first.
    pending: Vec<Pending>,
    /// Engine step records, kept so the run can be diagnosed at the end.
    execution: Vec<ExecutionStep>,
    next_seq: u64,
}

/// Records a [`RunTrace`] as a run executes.
///
/// Implements both [`StepInterceptor`] — which is how it sees a node's input and
/// bindings *before* the node runs — and
/// [`RunObserver`](crate::observability::RunObserver), which is how it learns
/// how long each activation took. Pass the same instance as both.
///
/// Attach a [`MockCaps`] to have the run's capability calls folded into the
/// trace and attributed to the node that made them.
pub struct RunTracer {
    state: Mutex<TraceState>,
    graph: Option<WorkflowGraph>,
    mocks: Option<std::sync::Arc<MockCaps>>,
}

impl RunTracer {
    /// A tracer for a run of `graph`.
    ///
    /// The graph is used to say whether a binding's upstream node exists and to
    /// find the nodes that never ran; a tracer without one still records every
    /// step.
    #[must_use]
    pub fn new(graph: Option<WorkflowGraph>) -> Self {
        Self {
            state: Mutex::new(TraceState::default()),
            graph,
            mocks: None,
        }
    }

    /// Fold `mocks`' capability calls into the trace, attributed per node.
    #[must_use]
    pub fn with_mocks(mut self, mocks: std::sync::Arc<MockCaps>) -> Self {
        self.mocks = Some(mocks);
        self
    }

    /// The trace recorded so far.
    #[must_use]
    pub fn trace(&self) -> RunTrace {
        let state = self.state.lock().expect("trace state poisoned");
        let calls = self
            .mocks
            .as_ref()
            .map(|m| m.log().calls())
            .unwrap_or_default();
        let diagnosis = match self.graph.as_ref() {
            Some(graph) => diagnose(graph, &state.execution),
            None => Diagnosis::default(),
        };
        RunTrace {
            steps: state.steps.clone(),
            calls,
            diagnosis,
        }
    }
}

#[async_trait::async_trait]
impl StepInterceptor for RunTracer {
    async fn intercept(&self, frame: StepFrame<'_>) -> StepAction {
        // Only the `After` phase is recorded: by then the activation has both
        // the input it received and the outcome it produced, so one frame
        // yields a whole step. Recording at `Before` too would mean carrying
        // half-built steps across a parallel super-step and matching them up
        // again, which the phases already do for free.
        if frame.phase == StepPhase::After {
            let bindings = trace_bindings(frame.node, self.graph.as_ref(), &frame.scope());
            let (output, port) = match frame.output {
                Some(out) => (out.items.clone(), out.port.clone()),
                None => (Vec::new(), None),
            };
            let mut state = self.state.lock().expect("trace state poisoned");
            let seq = state.next_seq;
            state.next_seq += 1;
            state.pending.push(Pending {
                step: TraceStep {
                    seq,
                    node_id: frame.node.id.clone(),
                    node_kind: frame.node.kind.clone(),
                    superstep: frame.step,
                    lane: frame.lane.map(|l| l.id.clone()),
                    attempts: frame.attempts,
                    input: frame.input.to_vec(),
                    output,
                    port,
                    status: if frame.error.is_some() {
                        TraceStatus::Error
                    } else {
                        TraceStatus::Success
                    },
                    error: frame.error.map(ToString::to_string),
                    // Filled in by `on_step_finish`, which fires immediately
                    // after this hook for the same activation.
                    duration_ms: 0,
                    bindings,
                },
            });
        }
        StepAction::Continue { state_patch: None }
    }

    fn capabilities_for(&self, node: &Node, capabilities: &Capabilities) -> Option<Capabilities> {
        // Hand this activation a bundle that stamps its node id on every call,
        // which is the only way the trace can say who called what.
        self.mocks
            .as_ref()
            .map(|mocks| mocks.capabilities_for_node(&node.id))
            .or_else(|| {
                let _ = capabilities;
                None
            })
    }
}

impl RunObserver for RunTracer {
    fn on_step_finish(&self, step: &ExecutionStep) {
        let mut state = self.state.lock().expect("trace state poisoned");
        state.execution.push(step.clone());
        // The engine fires this immediately after the `After` interception for
        // the same activation, so the oldest pending step for this node is the
        // one being reported. Matching by node id rather than position keeps a
        // parallel super-step's interleaving from mixing two nodes' timings.
        let found = state
            .pending
            .iter()
            .position(|p| p.step.node_id == step.node_id);
        match found {
            Some(index) => {
                let mut pending = state.pending.remove(index);
                pending.step.duration_ms = step.duration_ms;
                state.steps.push(pending.step);
            }
            None => {
                // No interceptor frame for this node — the tracer was installed
                // as an observer only. Record what the observer can see rather
                // than dropping the activation entirely.
                let seq = state.next_seq;
                state.next_seq += 1;
                state.steps.push(TraceStep {
                    seq,
                    node_id: step.node_id.clone(),
                    node_kind: NodeKind::Void,
                    superstep: 0,
                    lane: None,
                    attempts: 1,
                    input: Vec::new(),
                    output: Vec::new(),
                    port: None,
                    status: match step.status {
                        crate::observability::StepStatus::Success => TraceStatus::Success,
                        crate::observability::StepStatus::Error => TraceStatus::Error,
                    },
                    error: None,
                    duration_ms: step.duration_ms,
                    bindings: Vec::new(),
                });
            }
        }
    }
}

#[cfg(test)]
#[path = "trace_tests.rs"]
mod tests;
