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
//! });
//! assert_eq!(recorder.nodes.lock().unwrap().as_slice(), ["parse"]);
//! ```

use serde_json::Value;

/// The lifecycle status of a whole [`Run`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunStatus {
    /// The run is still executing (not yet driven to completion).
    Running,
    /// The run reached a terminal node and completed successfully.
    Completed,
    /// The run ended because a node failed under a `stop` error policy.
    Failed,
}

/// The outcome of a single [`ExecutionStep`].
#[derive(Debug, Clone)]
pub enum StepStatus {
    /// The node executed and produced output items.
    Success,
    /// The node's executor errored (after any retries were exhausted).
    Error,
}

/// One node activation within a [`Run`]: what the node produced, whether it
/// succeeded, and how long it took.
///
/// This is the record the canvas renders when a user inspects a node, and what a
/// run-history view summarizes.
#[derive(Debug, Clone)]
pub struct ExecutionStep {
    /// The id of the node this step ran.
    pub node_id: String,
    /// Whether the node succeeded or errored.
    pub status: StepStatus,
    /// The items the node emitted as a JSON value, or [`Value::Null`] on error.
    pub output: Value,
    /// Wall-clock duration of the node's executor, in milliseconds.
    pub duration_ms: u128,
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

    /// Called once per non-trigger node activation, as each step finishes.
    fn on_step_finish(&self, step: &ExecutionStep) {
        let _ = step;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_observer_callbacks_are_inert() {
        let observer = NoopObserver;
        observer.on_run_start("run-0");
        observer.on_step_finish(&ExecutionStep {
            node_id: "n".to_string(),
            status: StepStatus::Success,
            output: Value::Null,
            duration_ms: 0,
        });
        observer.on_run_finish(&Run {
            id: "run-0".to_string(),
            status: RunStatus::Completed,
            steps: Vec::new(),
        });
    }

    #[test]
    fn run_status_equality() {
        assert_eq!(RunStatus::Completed, RunStatus::Completed);
        assert_ne!(RunStatus::Completed, RunStatus::Failed);
    }
}
