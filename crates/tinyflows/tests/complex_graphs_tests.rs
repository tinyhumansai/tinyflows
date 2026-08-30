#![cfg(feature = "mock")]
//! Named, hand-built compositions whose ordering cannot be covered by shallow
//! generated graphs alone.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tinyflows::caps::ToolInvoker;
use tinyflows::caps::mock::mock_capabilities;
use tinyflows::compiler::compile;
use tinyflows::engine::{run_resumable, run_with_observer};
use tinyflows::model::{Edge, Node, NodeKind, WorkflowGraph};
use tinyflows::observability::RunObserver;

const GUARD: Duration = Duration::from_secs(10);

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

fn edge(from: &str, to: &str) -> Edge {
    port_edge(from, "main", to)
}

fn port_edge(from: &str, port: &str, to: &str) -> Edge {
    Edge {
        from_node: from.to_string(),
        from_port: port.to_string(),
        to_node: to.to_string(),
        to_port: "main".to_string(),
    }
}

#[derive(Default)]
struct Trace(Mutex<Vec<String>>);

impl RunObserver for Trace {
    fn on_step_finish(&self, step: &tinyflows::observability::ExecutionStep) {
        self.0
            .lock()
            .expect("trace mutex poisoned")
            .push(step.node_id.clone());
    }
}

fn child_transform() -> WorkflowGraph {
    WorkflowGraph {
        name: "lane_child".to_string(),
        nodes: vec![
            node("child_trigger", NodeKind::Trigger, Value::Null),
            node(
                "child_agent",
                NodeKind::Agent,
                json!({ "prompt": "refine this candidate" }),
            ),
            node(
                "child_tag",
                NodeKind::Transform,
                json!({ "set": { "child_complete": true } }),
            ),
        ],
        edges: vec![
            edge("child_trigger", "child_agent"),
            edge("child_agent", "child_tag"),
        ],
        ..Default::default()
    }
}

include!("complex_graphs/composition_tests.rs");
include!("complex_graphs/concurrency_and_failures_tests.rs");
