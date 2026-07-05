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
    /// Non-fatal data-binding diagnostics from the node's execution: every
    /// config `=`-expression that resolved to `null` (see
    /// [`crate::expr::resolve_traced`]). Lets a host's run view point at the
    /// exact unresolved wiring behind a bad tool call. Empty on error steps and
    /// for nodes without expression config.
    pub diagnostics: Vec<crate::expr::NullResolution>,
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
    use std::sync::{Arc, Mutex};

    #[test]
    fn noop_observer_callbacks_are_inert() {
        let observer = NoopObserver;
        observer.on_run_start("run-0");
        observer.on_step_finish(&ExecutionStep {
            node_id: "n".to_string(),
            status: StepStatus::Success,
            output: Value::Null,
            duration_ms: 0,
            diagnostics: Vec::new(),
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

    #[test]
    fn run_status_variants_are_distinct() {
        assert_ne!(RunStatus::Running, RunStatus::Completed);
        assert_ne!(RunStatus::Running, RunStatus::Failed);
        assert_ne!(RunStatus::Completed, RunStatus::Failed);
    }

    #[test]
    fn constructs_execution_step_with_each_status() {
        // `StepStatus` does not derive `PartialEq`, so match to inspect it.
        let ok = ExecutionStep {
            node_id: "parse".to_string(),
            status: StepStatus::Success,
            output: serde_json::json!([{ "json": { "x": 1 } }]),
            duration_ms: 12,
            diagnostics: Vec::new(),
        };
        assert_eq!(ok.node_id, "parse");
        assert_eq!(ok.duration_ms, 12);
        assert!(matches!(ok.status, StepStatus::Success));
        assert_eq!(ok.output, serde_json::json!([{ "json": { "x": 1 } }]));

        let err = ExecutionStep {
            node_id: "http".to_string(),
            status: StepStatus::Error,
            output: Value::Null,
            duration_ms: 0,
            diagnostics: Vec::new(),
        };
        assert!(matches!(err.status, StepStatus::Error));
        assert_eq!(err.output, Value::Null);
    }

    #[test]
    fn constructs_run_with_steps() {
        let run = Run {
            id: "run-7".to_string(),
            status: RunStatus::Completed,
            steps: vec![ExecutionStep {
                node_id: "a".to_string(),
                status: StepStatus::Success,
                output: serde_json::json!([]),
                duration_ms: 3,
                diagnostics: Vec::new(),
            }],
        };
        assert_eq!(run.id, "run-7");
        assert_eq!(run.status, RunStatus::Completed);
        assert_eq!(run.steps.len(), 1);
        assert_eq!(run.steps[0].node_id, "a");
    }

    #[test]
    fn cloned_run_is_independent() {
        let run = Run {
            id: "run-1".to_string(),
            status: RunStatus::Running,
            steps: Vec::new(),
        };
        let clone = run.clone();
        assert_eq!(clone.id, run.id);
        assert_eq!(clone.status, run.status);
    }

    /// A capturing observer used to assert the callbacks fire with the right
    /// records.
    #[derive(Default)]
    struct Capture {
        started: Mutex<Vec<String>>,
        steps: Mutex<Vec<String>>,
        finished: Mutex<Vec<(String, RunStatus)>>,
    }

    impl RunObserver for Capture {
        fn on_run_start(&self, run_id: &str) {
            self.started.lock().unwrap().push(run_id.to_string());
        }

        fn on_step_finish(&self, step: &ExecutionStep) {
            self.steps.lock().unwrap().push(step.node_id.clone());
        }

        fn on_run_finish(&self, run: &Run) {
            self.finished
                .lock()
                .unwrap()
                .push((run.id.clone(), run.status.clone()));
        }
    }

    #[test]
    fn custom_observer_receives_all_callbacks() {
        let observer = Capture::default();

        observer.on_run_start("run-9");
        observer.on_step_finish(&ExecutionStep {
            node_id: "first".to_string(),
            status: StepStatus::Success,
            output: serde_json::json!([]),
            duration_ms: 1,
            diagnostics: Vec::new(),
        });
        observer.on_step_finish(&ExecutionStep {
            node_id: "second".to_string(),
            status: StepStatus::Error,
            output: Value::Null,
            duration_ms: 2,
            diagnostics: Vec::new(),
        });
        observer.on_run_finish(&Run {
            id: "run-9".to_string(),
            status: RunStatus::Failed,
            steps: Vec::new(),
        });

        assert_eq!(observer.started.lock().unwrap().as_slice(), ["run-9"]);
        assert_eq!(
            observer.steps.lock().unwrap().as_slice(),
            ["first", "second"]
        );
        assert_eq!(
            observer.finished.lock().unwrap().as_slice(),
            [("run-9".to_string(), RunStatus::Failed)]
        );
    }

    #[test]
    fn observer_is_usable_as_trait_object() {
        // The engine holds the observer as `Arc<dyn RunObserver>`; confirm a
        // custom impl coerces and dispatches dynamically.
        let observer: Arc<dyn RunObserver> = Arc::new(Capture::default());
        observer.on_run_start("run-dyn");
        observer.on_step_finish(&ExecutionStep {
            node_id: "n".to_string(),
            status: StepStatus::Success,
            output: serde_json::json!([]),
            duration_ms: 0,
            diagnostics: Vec::new(),
        });
    }
}
