//! Drives a [`CompiledWorkflow`] to completion by lowering it onto the in-crate
//! state-graph runtime ([`crate::graph`]).
//!
//! `run` builds a fresh [`crate::graph`] state graph from the validated
//! [`WorkflowGraph`](crate::model::WorkflowGraph) — capturing the run's host
//! [`Capabilities`] in each node handler — then drives it and returns the final
//! run state. State is a [`serde_json::Value`] laid out as
//! `{ "run": { "trigger": …, "inputs": { … } }, "nodes": { "<id>": { "items": [ … ] } } }`;
//! a merge reducer folds each node's item output into that map. `run.trigger` is
//! the free-form payload that fired the run; `run.inputs` is the workflow's
//! resolved declared inputs (see [`RunInput`]), which node config addresses as
//! `=inputs.<name>`.
//!
//! Lowering covers the **linear** path (one successor per node), **conditional
//! branching** (successors on distinct ports), **parallel fan-out** (several
//! successors sharing one port, driven by a `Command::goto` that activates every
//! branch concurrently), and a **fan-in barrier** (any node with more than one
//! predecessor is wired with waiting edges so it runs only once all of them
//! finish).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::graph::{
    Command, CompiledGraph, END, GraphBuilder, GraphError, Interrupt, NodeResult, RouteTarget,
    StateReducer,
};
use serde_json::{Map, Value, json};

/// Checkpointer types re-exported from [`crate::graph`] so a host can name and
/// implement them without reaching into the runtime module directly.
///
/// A host that wants durable, cross-process HITL resume implements
/// [`Checkpointer<serde_json::Value>`] (or reuses [`FileCheckpointer`]) and
/// injects it via [`run_with_checkpointer`] / [`resume_with_checkpointer`]. The
/// engine keys persisted state by a caller-supplied `thread_id`.
///
/// [`InMemoryCheckpointer`] is the process-local default used by [`run`],
/// [`run_with_observer`], [`run_resumable`], and [`resume`]; [`DurabilityMode`]
/// configures how aggressively a checkpointer persists.
pub use crate::graph::{Checkpointer, DurabilityMode, FileCheckpointer, InMemoryCheckpointer};

/// Graph-observability types re-exported from [`crate::graph`] so a host can
/// journal a run's durable [`GraphObservation`]s without taking a direct
/// dependency on the runtime module.
///
/// Inject a [`GraphEventJournal`] via [`run_with_checkpointer_journaled`] /
/// [`resume_with_checkpointer_journaled`]; every graph event the run emits is
/// wrapped into a [`GraphObservation`] and appended under the run's
/// graph run id (returned on [`JournaledRunOutcome`]), so the host can
/// read the slice back (`journal.read_from(run_id, 0)`) and e.g. export it to
/// Langfuse. [`InMemoryGraphEventJournal`] is a process-local implementation
/// suitable for per-run capture.
pub use crate::graph::{GraphEventJournal, GraphObservation, InMemoryGraphEventJournal};

use crate::caps::Capabilities;
use crate::compiler::CompiledWorkflow;
use crate::data::Item;
use crate::error::{EngineError, Result, ValidationError};
use crate::interception::{StepAction, StepFrame, StepInterceptor, StepPhase};
use crate::model::NodeKind;
use crate::nodes::{NodeContext, executor_for};
use crate::observability::{ExecutionStep, Run, RunObserver, RunStatus, StepStatus};

/// Source of process-local run ids. Monotonic and cheap; deliberately not
/// time- or random-based so ids stay deterministic within a process.
static NEXT_RUN_ID: AtomicU64 = AtomicU64::new(0);

/// A cooperative cancellation signal for a workflow run.
///
/// Cheap to clone (an [`Arc`] around an atomic flag) and runtime-agnostic — the
/// crate deliberately avoids depending on any executor's cancellation type. Hand
/// a clone to a cancellable entry point ([`run_cancellable`] /
/// [`resume_cancellable`]) and keep another; calling [`cancel`](Self::cancel)
/// from anywhere flips the flag, and the run stops scheduling real node work at
/// the next node boundary, returning a [`RunOutcome`] with
/// [`cancelled`](RunOutcome::cancelled) set.
///
/// Cancellation is **cooperative and boundary-level**: a node already executing
/// runs to completion; the token is checked before each node runs, so no *new*
/// node work starts after cancellation. This complements (does not replace) a
/// host's hard task-abort — it lets a run wind down cleanly rather than being
/// dropped mid-await.
#[derive(Debug, Clone, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    /// Creates a fresh, un-cancelled token.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Signals cancellation. Idempotent; safe to call from any thread.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Whether cancellation has been signalled.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// What a caller hands a run: the trigger payload plus values for the
/// workflow's declared inputs.
///
/// The two are deliberately separate channels. The **trigger payload** is
/// whatever fired the run — a webhook body, a chat message, an empty object for
/// a manual start — and is free-form by nature. **Inputs** are the workflow's
/// declared, typed parameters (see [`crate::model::WorkflowInput`]); they are
/// validated against the graph's declarations before anything executes.
///
/// Every entry point takes `impl Into<RunInput>`, and [`Value`] converts, so a
/// caller with no declared inputs passes a bare payload exactly as before:
///
/// ```
/// use tinyflows::engine::RunInput;
/// use serde_json::json;
///
/// // Trigger payload only — the historical form.
/// let plain: RunInput = json!({"from": "webhook"}).into();
/// assert!(plain.inputs.is_empty());
///
/// // With declared input values.
/// let mut values = serde_json::Map::new();
/// values.insert("repo".into(), json!("acme/api"));
/// let parameterized = RunInput::new(json!({})).with_inputs(values);
/// assert_eq!(parameterized.inputs["repo"], json!("acme/api"));
/// ```
#[derive(Debug, Clone, Default)]
pub struct RunInput {
    /// The trigger payload, seeded as the trigger node's item and `run.trigger`.
    pub trigger: Value,
    /// Caller-supplied values for the workflow's declared inputs, by name.
    /// Validated by [`crate::model::resolve_inputs`] before the run starts.
    pub inputs: Map<String, Value>,
    /// Gate ids pre-approved for this run.
    ///
    /// An explicit channel, separate from the trigger payload. Approvals can
    /// also be written as `trigger.approvals` when the trigger happens to be an
    /// object, and that remains supported — but a run whose trigger is an array
    /// or a scalar (a `sub_workflow` child is seeded with its input *items*, an
    /// array) has nowhere to put them, so smuggling approvals through the
    /// payload cannot work in general.
    pub approvals: Vec<String>,
    /// A durable, **host-generated** identity for this run, seeded into the run
    /// state as `run.id`.
    ///
    /// The engine keeps its own process-local run id for observability, but
    /// that one is minted fresh inside every `run` call — and a resume
    /// re-executes the workflow, so it changes between a run and its own
    /// resume. Anything that must name *this* run across a pause therefore
    /// cannot use it, and only the host (which owns run persistence) knows an
    /// id that survives.
    ///
    /// Today's consumer is the [`approval`](crate::nodes::integration::approval)
    /// node, whose `request_id` defaults to `"<run id>:<node id>"` — the key the
    /// host's [`ApprovalProvider`](crate::caps::ApprovalProvider) de-duplicates
    /// reviews on. Unique per run *and* stable across resume is exactly the
    /// property that makes one human review, rather than a fresh card every
    /// time the run is looked at.
    ///
    /// **Must be server-generated.** It lands in `run.id`, outside the
    /// caller-supplied trigger payload, precisely so it is not attacker
    /// influenced; copying a request field into it hands an attacker the
    /// de-duplication key and, with it, an earlier run's approval.
    ///
    /// `None` leaves `run.id` unset, which is what every caller predating this
    /// meant.
    pub run_id: Option<String>,
}

impl RunInput {
    /// A run carrying only a trigger payload and no declared-input values.
    #[must_use]
    pub fn new(trigger: Value) -> Self {
        Self {
            trigger,
            inputs: Map::new(),
            approvals: Vec::new(),
            run_id: None,
        }
    }

    /// Attaches values for the workflow's declared inputs.
    #[must_use]
    pub fn with_inputs(mut self, inputs: Map<String, Value>) -> Self {
        self.inputs = inputs;
        self
    }

    /// Pre-approves the named gates for this run (see [`Self::approvals`]).
    #[must_use]
    pub fn with_approvals(mut self, approvals: Vec<String>) -> Self {
        self.approvals = approvals;
        self
    }

    /// Names this run with a durable, host-generated id (see [`Self::run_id`]).
    ///
    /// Pass the **same** id when resuming the run: that is what makes a paused
    /// human review resolve to the one already in front of a person instead of
    /// opening a second one.
    #[must_use]
    pub fn with_run_id(mut self, run_id: impl Into<String>) -> Self {
        self.run_id = Some(run_id.into());
        self
    }
}

impl From<Value> for RunInput {
    /// Treats a bare JSON value as a trigger payload with no declared inputs —
    /// what every caller meant before inputs existed.
    fn from(trigger: Value) -> Self {
        Self::new(trigger)
    }
}

/// The result of a completed workflow run.
#[derive(Debug, Clone)]
pub struct RunOutcome {
    /// The final run state after the terminal node(s) completed.
    pub output: Value,
    /// Node ids that paused the run awaiting human approval. A node is listed
    /// here when it is an approval gate (`config.requires_approval == true`)
    /// whose id was not present in the run input's `approvals` array; its
    /// downstream did not run. Empty for a fully completed run.
    pub pending_approvals: Vec<String>,
    /// Whether the run observed a cancelled [`CancellationToken`] and wound down
    /// early. When `true`, some downstream nodes were skipped (their slots in
    /// `output` were not produced), so treat `output` as partial. Always `false`
    /// for runs started without a token or that completed before any cancel.
    pub cancelled: bool,
}

/// The runtime-minted identifiers of the underlying graph run.
///
/// A [`GraphEventJournal`] attached to a run keys that run's
/// [`GraphObservation`]s by `run_id`, so a host that journaled a run reads the
/// slice back with `journal.read_from(&run_id, 0)`. `root_run_id` is the root
/// of the recursion tree (equal to `run_id` for a top-level run) and is what
/// Langfuse-style exporters default their trace id to.
#[derive(Debug, Clone)]
pub struct GraphRunIds {
    /// The run id of this graph execution — the journal's stream key.
    pub run_id: String,
    /// The root run id of the recursion tree (equals `run_id` at top level).
    pub root_run_id: String,
}

/// The result of a journaled workflow run: the plain [`RunOutcome`] plus the
/// [`GraphRunIds`] needed to read the run's [`GraphObservation`]s back out of
/// the journal the caller injected.
#[derive(Debug, Clone)]
pub struct JournaledRunOutcome {
    /// The workflow-level outcome (final state + pending approval gates).
    pub outcome: RunOutcome,
    /// The graph run ids the injected journal keys observations by.
    pub graph_run_ids: GraphRunIds,
}

mod state;
pub(crate) use state::replace;
#[cfg(test)]
pub(crate) use state::{LANE_KEY, REPLACE};
use state::{
    MergeReducer, collect_input, collect_input_since, lane_context, lane_envelope, lane_input,
    lane_items_update, merge, stamp_activation_step,
};

mod routing;
pub use routing::back_edges;
use routing::{
    HandlerRouting, conditional_predecessors, error_item, find_conditional_brancher,
    handler_routing, items_update, items_update_with_meta, outgoing_by_port,
};

mod api;
pub(crate) use api::run_sub_workflow;
pub use api::{
    MAX_GRAPH_CONCURRENCY, MAX_SUB_WORKFLOW_DEPTH, run, run_cancellable,
    run_cancellable_with_observer, run_with_observer,
};

mod build;
use build::build_graph;

mod run_config;
use run_config::RunConfig;

mod run_state;
pub use run_state::resume;
use run_state::{build_and_run, default_thread_id, merge_approvals};

mod resumable;
pub use resumable::*;

#[cfg(test)]
#[path = "merge_tests.rs"]
mod merge_tests;

#[cfg(test)]
#[path = "engine_merge_tests.rs"]
mod merge_property_tests;

#[cfg(test)]
#[path = "lane_context_tests.rs"]
mod lane_context_tests;

#[cfg(test)]
#[path = "engine_tests.rs"]
mod tests;
