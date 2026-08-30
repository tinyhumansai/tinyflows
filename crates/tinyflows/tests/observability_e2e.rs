#![cfg(feature = "mock")]
//! End-to-end test for the [`RunObserver`] observability hooks.
//!
//! A host receives live run/step records by implementing [`RunObserver`] and
//! passing it to [`run_with_observer`]. This test installs a capturing observer
//! over a multi-node flow and asserts the recorded steps (their count, node ids,
//! and per-step statuses) plus that `on_run_start` and `on_run_finish` each fire
//! exactly once, with the assembled [`Run`] reporting a completed status.
//!
//! Gated behind the `mock` feature, so plain `cargo test` skips it while
//! `cargo test --all-features` runs it.

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tinyflows::caps::mock::mock_capabilities;
use tinyflows::compiler::compile;
use tinyflows::engine::run_with_observer;
use tinyflows::model::{Edge, Node, NodeKind, TriggerKind, WorkflowGraph};
use tinyflows::observability::{ExecutionStep, Run, RunObserver, RunStatus, StepStatus};

/// Builds a node with the given id, kind, and config.
fn node(id: &str, kind: NodeKind, config: Value) -> Node {
    Node {
        id: id.to_string(),
        kind,
        type_version: 1,
        name: id.to_string(),
        config,
        ports: vec![],
        position: None,
    }
}

/// Builds a trigger node with the given firing mode.
fn trigger(id: &str, kind: TriggerKind) -> Node {
    node(id, NodeKind::Trigger, json!({ "kind": kind }))
}

/// Builds an edge from `from_node`'s `main` port into `to_node`'s `main` port.
fn edge(from_node: &str, to_node: &str) -> Edge {
    Edge {
        from_node: from_node.to_string(),
        from_port: "main".to_string(),
        to_node: to_node.to_string(),
        to_port: "main".to_string(),
    }
}

/// What the capturing observer records across a run.
#[derive(Default)]
struct Recorded {
    run_starts: u32,
    run_finishes: u32,
    /// (node_id, was_success) per finished step, in the order received.
    steps: Vec<(String, bool)>,
    /// The terminal status carried by the assembled `Run` on finish.
    finish_status: Option<RunStatus>,
    /// The step count carried by the assembled `Run` on finish.
    finish_step_count: Option<usize>,
}

/// A [`RunObserver`] that records every hook it receives into a shared [`Recorded`].
struct Capture {
    recorded: Arc<Mutex<Recorded>>,
}

impl RunObserver for Capture {
    fn on_run_start(&self, _run_id: &str) {
        self.recorded.lock().expect("lock").run_starts += 1;
    }

    fn on_step_finish(&self, step: &ExecutionStep) {
        let success = matches!(step.status, StepStatus::Success);
        self.recorded
            .lock()
            .expect("lock")
            .steps
            .push((step.node_id.clone(), success));
    }

    fn on_run_finish(&self, run: &Run) {
        let mut recorded = self.recorded.lock().expect("lock");
        recorded.run_finishes += 1;
        recorded.finish_status = Some(run.status.clone());
        recorded.finish_step_count = Some(run.steps.len());
    }
}

/// A multi-node flow reports one `on_step_finish` per non-trigger node, fires
/// `on_run_start`/`on_run_finish` exactly once, and the assembled `Run` records a
/// completed status with a step per non-trigger node.
#[tokio::test]
async fn observer_captures_steps_and_lifecycle() {
    // trigger -> transform -> output_parser -> tool_call: three non-trigger nodes.
    let graph = WorkflowGraph {
        name: "observed".to_string(),
        nodes: vec![
            trigger("start", TriggerKind::Manual),
            node(
                "label",
                NodeKind::Transform,
                json!({ "set": { "seen": true } }),
            ),
            node("parse", NodeKind::OutputParser, Value::Null),
            node(
                "call",
                NodeKind::ToolCall,
                json!({ "slug": "slack.post", "args": {} }),
            ),
        ],
        edges: vec![
            edge("start", "label"),
            edge("label", "parse"),
            edge("parse", "call"),
        ],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let caps = mock_capabilities();

    let recorded = Arc::new(Mutex::new(Recorded::default()));
    let observer: Arc<dyn RunObserver> = Arc::new(Capture {
        recorded: recorded.clone(),
    });

    run_with_observer(&compiled, json!({ "x": 1 }), &caps, &observer)
        .await
        .expect("run");

    let recorded = recorded.lock().expect("lock");

    // Lifecycle hooks each fired exactly once.
    assert_eq!(recorded.run_starts, 1, "on_run_start should fire once");
    assert_eq!(recorded.run_finishes, 1, "on_run_finish should fire once");

    // One step per non-trigger node (the trigger itself does not emit a step).
    assert_eq!(
        recorded.steps.len(),
        3,
        "one step should be recorded per non-trigger node, got {:?}",
        recorded.steps
    );
    let ids: Vec<&str> = recorded.steps.iter().map(|(id, _)| id.as_str()).collect();
    assert!(
        ids.contains(&"label") && ids.contains(&"parse") && ids.contains(&"call"),
        "steps should cover every non-trigger node, got {ids:?}"
    );
    assert!(
        !ids.contains(&"start"),
        "the trigger node should not produce a step"
    );

    // Every step succeeded.
    assert!(
        recorded.steps.iter().all(|(_, success)| *success),
        "every step should have a Success status, got {:?}",
        recorded.steps
    );

    // The assembled Run handed to on_run_finish reflects a completed run.
    assert_eq!(
        recorded.finish_status,
        Some(RunStatus::Completed),
        "the finished run should report Completed status"
    );
    assert_eq!(
        recorded.finish_step_count,
        Some(3),
        "the assembled Run should carry one step per non-trigger node"
    );
}
