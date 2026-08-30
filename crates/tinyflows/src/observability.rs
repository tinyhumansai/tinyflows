//! Run/step observability records and the host-facing [`RunObserver`] hook.
//!
//! tinyflows **emits** structured records of what happened during a run; the
//! host **persists** and renders them — the crate never owns a database.
//! A [`Run`] captures one workflow
//! execution as a sequence of [`ExecutionStep`]s, one per non-trigger node
//! activation, each recording the node's status, output items, and wall-clock
//! duration.
//!
//! A host receives these live by implementing [`RunObserver`] and passing it to
//! [`crate::engine::run_with_observer`]. Every callback has a default no-op body,
//! so a host implements only the hooks it cares about. The default
//! [`crate::engine::run`] path installs a [`NoopObserver`], so observability adds
//! no cost unless a host opts in.
//!
//! ```
//! use std::sync::{Arc, Mutex};
//! use tinyflows::observability::{ExecutionStep, Run, RunObserver, StepStatus};
//!
//! #[derive(Default)]
//! struct Recorder {
//!     nodes: Mutex<Vec<String>>,
//! }
//!
//! impl RunObserver for Recorder {
//!     fn on_step_finish(&self, step: &ExecutionStep) {
//!         self.nodes.lock().unwrap().push(step.node_id.clone());
//!     }
//! }
//!
//! let recorder = Recorder::default();
//! recorder.on_step_finish(&ExecutionStep {
//!     node_id: "parse".to_string(),
//!     status: StepStatus::Success,
//!     output: serde_json::json!([]),
//!     duration_ms: 0,
//!     diagnostics: vec![],
//!     transcript: vec![],
//! });
//! assert_eq!(recorder.nodes.lock().unwrap().as_slice(), ["parse"]);
//! ```

use serde_json::Value;

/// The lifecycle status of a whole [`Run`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunStatus {
    /// The run is still executing (not yet driven to completion).
    Running,
    /// The run reached a terminal node and every node completed cleanly.
    Completed,
    /// The run reached a terminal node, but at least one node failed under a
    /// non-`stop` error policy (`continue` or `route`): its failure was turned
    /// into data and the run proceeded, so the run did not [`Failed`], yet it
    /// did not complete cleanly either. Without this a `continue`/`route`
    /// failure is invisible in the terminal status and a host reads success
    /// while a node failed (#661 L1). The failing nodes are exactly the
    /// [`Run::steps`] whose [`StepStatus`] is [`StepStatus::Error`].
    ///
    /// [`Failed`]: RunStatus::Failed
    CompletedWithErrors,
    /// The run ended because a node failed under a `stop` error policy.
    Failed,
}

/// The outcome of a single [`ExecutionStep`].
///
/// [`Success`](Self::Success) is the default so [`ExecutionStep`] can derive
/// one — see the note there.
#[derive(Debug, Clone, Default)]
pub enum StepStatus {
    /// The node executed and produced output items.
    #[default]
    Success,
    /// The node's executor errored (after any retries were exhausted).
    Error,
}

/// One node activation within a [`Run`]: what the node produced, whether it
/// succeeded, and how long it took.
///
/// This is the record the canvas renders when a user inspects a node, and what a
/// run-history view summarizes.
/// # Constructing one
///
/// `Default` is derived so a host can build a step without naming every field:
///
/// ```
/// use tinyflows::observability::{ExecutionStep, StepStatus};
///
/// let step = ExecutionStep {
///     node_id: "solve".to_string(),
///     status: StepStatus::Error,
///     ..Default::default()
/// };
/// assert!(step.transcript.is_empty());
/// ```
///
/// That is the migration path when this struct gains a field: a literal that
/// names every one breaks, and `..Default::default()` does not. Preferred over
/// `#[non_exhaustive]`, which would forbid the literal outright — worse for a
/// type hosts are meant to build in their own tests.
#[derive(Debug, Clone, Default)]
pub struct ExecutionStep {
    /// The id of the node this step ran.
    pub node_id: String,
    /// Whether the node succeeded or errored.
    pub status: StepStatus,
    /// The items the node emitted as a JSON value, or [`Value::Null`] on error.
    pub output: Value,
    /// Wall-clock duration of the node's executor, in milliseconds.
    pub duration_ms: u128,
    /// Non-fatal data-binding diagnostics from the node's execution: every
    /// config `=`-expression that resolved to `null` (see
    /// [`crate::expr::resolve_traced`]). Lets a host's run view point at the
    /// exact unresolved wiring behind a bad tool call. Empty on error steps and
    /// for nodes without expression config.
    pub diagnostics: Vec<crate::expr::NullResolution>,
    /// What a harness did inside this node, in order — the thinking, the tool
    /// calls, the results.
    ///
    /// [`output`](Self::output) says what came *out* of the node; this says
    /// what happened *inside* it, which for an `agent` node is most of what
    /// there is to know. Folded by the host and handed over on
    /// [`AgentRunOutcome::transcript`](crate::caps::AgentRunOutcome::transcript);
    /// the engine copies it here and never interprets it.
    ///
    /// Empty for every non-`agent` node, for a host that reports none, and on
    /// an error step — a node that failed produced no outcome to read one from,
    /// which is a known gap for a paused agent (see `AgentRunOutcome::transcript`).
    ///
    /// **Settled, not live.** Entries arrive when the node finishes, because
    /// they ride the outcome the harness returns. Reporting them *during* a run
    /// would need a sink on the capability contract, which
    /// [`AgentRunRequest`](crate::caps::AgentRunRequest) cannot carry (it is
    /// `Serialize` + `PartialEq`); that is a deliberate follow-up rather than
    /// something to imply here.
    pub transcript: Vec<crate::transcript::TranscriptEntry>,
}

/// One execution of a workflow, captured as an ordered list of [`ExecutionStep`]s.
///
/// The engine emits steps live via [`RunObserver::on_step_finish`] and hands the
/// assembled `Run` to [`RunObserver::on_run_finish`] once the run settles.
#[derive(Debug, Clone)]
pub struct Run {
    /// Unique id for this run (process-local, e.g. `"run-3"`).
    pub id: String,
    /// The run's terminal status.
    pub status: RunStatus,
    /// The per-node steps, in the order they finished.
    pub steps: Vec<ExecutionStep>,
}

impl Run {
    /// The ids of the nodes that errored in this run — the [`steps`](Run::steps)
    /// whose [`StepStatus`] is [`StepStatus::Error`], in the order they finished.
    ///
    /// Always empty for a clean [`RunStatus::Completed`], and always non-empty
    /// for [`RunStatus::CompletedWithErrors`] (that status *is* derived from at
    /// least one `Error` step: every node a `continue`/`route` policy turned
    /// into data records one).
    ///
    /// For [`RunStatus::Failed`] it names the failing node **only when a node
    /// caused the failure** — a `stop`-policy node records its `Error` step on
    /// the way out before ending the run. A `Failed` run that came from a
    /// driver-level fault with no node behind it (a hit recursion limit, a
    /// checkpointer error, a graph-runtime error) records no `Error` step and
    /// so returns an **empty** vector. So read a non-empty result as "these
    /// nodes failed" — never read emptiness as "the run succeeded", and never
    /// use this as a proxy for [`RunStatus::Failed`]; consult the run's
    /// [`status`](Run::status) for the outcome. Lets a host act on *which* nodes
    /// failed without scanning every step itself.
    pub fn failed_node_ids(&self) -> Vec<&str> {
        self.steps
            .iter()
            .filter(|step| matches!(step.status, StepStatus::Error))
            .map(|step| step.node_id.as_str())
            .collect()
    }
}

/// A host-implemented hook that receives run/step records as a run executes.
///
/// Every method has a default no-op body, so a host overrides only the callbacks
/// it needs. Implementations must be `Send + Sync`: the engine clones the
/// observer (as `Arc<dyn RunObserver>`) into each node handler, which run across
/// threads. Hosts may apply redaction here before persisting or logging.
pub trait RunObserver: Send + Sync {
    /// Called once, before any node runs, with the new run's id.
    fn on_run_start(&self, run_id: &str) {
        let _ = run_id;
    }

    /// Called once per non-trigger node activation, immediately before its
    /// first execution attempt.
    fn on_step_start(&self, node_id: &str) {
        let _ = node_id;
    }

    /// Called once per non-trigger node activation, as each step finishes.
    fn on_step_finish(&self, step: &ExecutionStep) {
        let _ = step;
    }

    /// Called as each item of a per-item node starts.
    ///
    /// `index` is the stable input position and `total` is the batch size. The
    /// callbacks are silent for nodes that execute once, so hosts can use them
    /// specifically to render fan-out work in flight.
    fn on_item_start(&self, node_id: &str, index: usize, total: usize) {
        let _ = (node_id, index, total);
    }

    /// Called as each item of a per-item node settles.
    ///
    /// `ok` describes that item's work before `on_item_error` policy is applied;
    /// a collected item error therefore reports `false` even when the overall
    /// node succeeds. Fail-fast cancellation may drop started items before this
    /// callback can run.
    fn on_item_finish(&self, node_id: &str, index: usize, total: usize, ok: bool) {
        let _ = (node_id, index, total, ok);
    }

    /// Called once, after the run settles, with the assembled [`Run`] record.
    fn on_run_finish(&self, run: &Run) {
        let _ = run;
    }
}

/// A [`RunObserver`] that ignores every callback.
///
/// Installed by [`crate::engine::run`] so the default run path carries no
/// observability overhead.
pub struct NoopObserver;

impl RunObserver for NoopObserver {}

/// A [`RunObserver`] that keeps every finished step.
///
/// The reason to keep them is `ExecutionStep::diagnostics`: the engine records
/// there which `=`-expressions resolved to null while it was resolving a node's
/// config, and that is the one place a graph's real wiring failure is visible.
/// A run does not fail on a null — null is a legal value — so the only way to
/// learn that a step ran on nothing is to watch it go by.
///
/// Steps are pushed synchronously from `on_step_finish`, so once the run future
/// resolves, every step it will ever record is already here.
#[derive(Debug, Default)]
pub struct CapturingObserver {
    steps: std::sync::Mutex<Vec<ExecutionStep>>,
}

impl RunObserver for CapturingObserver {
    fn on_step_finish(&self, step: &ExecutionStep) {
        self.steps
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(step.clone());
    }
}

impl CapturingObserver {
    /// A snapshot of every step recorded so far.
    #[must_use]
    pub fn steps(&self) -> Vec<ExecutionStep> {
        self.steps
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

#[cfg(test)]
#[path = "observability_tests.rs"]
mod tests;
