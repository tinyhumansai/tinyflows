#![cfg(feature = "mock")]
//! End-to-end tests for bounded loops: the `loop` node kind and the back-edge
//! lowering that makes any cycle iterate instead of deadlocking.
//!
//! These drive real graphs through the real engine on the deterministic mock
//! capabilities. The loop body is built from pure control-flow nodes
//! (`output_parser` passthroughs, `transform`) so no capability needs swapping
//! and the assertions are about routing, not about what a host returned.
//!
//! **Every test wraps its run in a `tokio::time::timeout`.** A loop bug is
//! exactly the kind that hangs rather than fails, and a hung test takes the
//! whole suite with it — the guard turns that into a named failure.
//!
//! Gated behind the `mock` cargo feature so plain `cargo test` skips it while
//! `cargo test --all-features` runs it.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use tinyflows::caps::mock::mock_capabilities;
use tinyflows::compiler::compile;
use tinyflows::engine::run;
use tinyflows::error::EngineError;
use tinyflows::error::ValidationError;
use tinyflows::model::{Edge, Node, NodeKind, WorkflowGraph};
use tinyflows::observability::RunObserver;

/// How long any single run in this file may take before it is called a hang.
const GUARD: Duration = Duration::from_secs(10);

/// Records the order nodes finished in, so a test can assert on sequencing that
/// the final state cannot show.
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

/// Builds a node with the given id, kind, and config (no ports, no position).
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

/// Builds a `main` -> `main` edge from `from` to `to`.
fn edge(from: &str, to: &str) -> Edge {
    Edge {
        from_node: from.to_string(),
        from_port: "main".to_string(),
        to_node: to.to_string(),
        to_port: "main".to_string(),
    }
}

/// Builds an edge leaving `from` on a named port (`body`, `done`, …).
fn port_edge(from: &str, from_port: &str, to: &str) -> Edge {
    Edge {
        from_node: from.to_string(),
        from_port: from_port.to_string(),
        to_node: to.to_string(),
        to_port: "main".to_string(),
    }
}

/// The canonical shape: `t -> l --body--> work --> (back to l)`, `l --done--> out`.
///
/// `loop_config` is the `loop` node's config, which is what each test varies.
fn loop_graph(loop_config: Value) -> WorkflowGraph {
    WorkflowGraph {
        name: "bounded_loop".to_string(),
        nodes: vec![
            node("t", NodeKind::Trigger, Value::Null),
            node("l", NodeKind::Loop, loop_config),
            node("work", NodeKind::OutputParser, Value::Null),
            node("out", NodeKind::OutputParser, Value::Null),
        ],
        edges: vec![
            edge("t", "l"),
            port_edge("l", "body", "work"),
            edge("work", "l"),
            port_edge("l", "done", "out"),
        ],
        ..Default::default()
    }
}

/// Runs a graph under the hang guard, returning the engine's result.
async fn run_guarded(
    graph: &WorkflowGraph,
) -> tinyflows::error::Result<tinyflows::engine::RunOutcome> {
    let caps = mock_capabilities();
    let compiled = compile(graph).expect("compile");
    match tokio::time::timeout(GUARD, run(&compiled, json!({}), &caps)).await {
        Err(_elapsed) => panic!("run hung past {GUARD:?} — the loop did not terminate"),
        Ok(inner) => inner,
    }
}

include!("loop_e2e/loop_e2e_part_01_tests.rs");
include!("loop_e2e/loop_e2e_part_02_tests.rs");
