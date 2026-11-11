#![cfg(feature = "mock")]
//! A loop whose body scatters: each pass gathers only its own lanes.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tinyflows::caps::ToolInvoker;
use tinyflows::caps::mock::mock_capabilities;
use tinyflows::compiler::compile;
use tinyflows::model::{Edge, Node, NodeKind, WorkflowGraph};

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

/// Returns its arguments, so each lane's output is its candidate.
struct Echo;

#[async_trait::async_trait]
impl ToolInvoker for Echo {
    async fn invoke(
        &self,
        _slug: &str,
        args: Value,
        _conn: Option<&str>,
    ) -> tinyflows::error::Result<Value> {
        Ok(args)
    }
}

/// A refinement loop whose body scatters, with `candidates` proposed per pass.
///
/// `candidates` is the expression `propose` sets per pass; each candidate is
/// evaluated in its own lane, gathered, and folded into the loop state.
fn refinement_loop(candidates: &str, loop_config: Value) -> WorkflowGraph {
    WorkflowGraph {
        name: "refinement_loop_scatter".to_string(),
        nodes: vec![
            node(
                "t",
                NodeKind::Trigger,
                json!({ "recursion_limit": 200, "max_node_visits": 100 }),
            ),
            node("refine", NodeKind::Loop, loop_config),
            node(
                "propose",
                NodeKind::Transform,
                json!({ "set": { "candidates": candidates } }),
            ),
            node(
                "scatter",
                NodeKind::Scatter,
                json!({ "path": "candidates" }),
            ),
            node(
                "evaluate",
                NodeKind::ToolCall,
                json!({ "slug": "refine.evaluate", "args": "=item" }),
            ),
            node(
                "gather",
                NodeKind::Gather,
                json!({ "from": ["evaluate"], "poll_interval_ms": 1 }),
            ),
            node("out", NodeKind::OutputParser, Value::Null),
        ],
        edges: vec![
            edge("t", "refine"),
            port_edge("refine", "body", "propose"),
            edge("propose", "scatter"),
            edge("scatter", "evaluate"),
            edge("evaluate", "gather"),
            edge("gather", "refine"),
            port_edge("refine", "done", "out"),
        ],
        ..Default::default()
    }
}

fn gathered_candidates(output: &Value) -> Vec<i64> {
    output["nodes"]["gather"]["items"]
        .as_array()
        .expect("gather items")
        .iter()
        .filter_map(|item| item["json"]["json"]["c"].as_i64())
        .collect()
}

/// A narrower second pass must not gather the first pass's extra lane.
///
/// Lane slots are never cleared between passes, so after pass 2 (lanes
/// `scatter#0`, `scatter#1`) `evaluate`'s `scatter#2` slot from pass 1 is still
/// in state. The gather counts only lanes stamped with the scatter's current
/// activation, so it releases `[20, 21]` rather than `[20, 21, 12]`.
#[tokio::test]
async fn a_narrower_pass_does_not_gather_stale_lanes() {
    let graph = refinement_loop(
        "=if .nodes.refine.iteration == 1 then [{c: 10}, {c: 11}, {c: 12}] \
         else [{c: 20}, {c: 21}] end",
        json!({ "max_iterations": 2, "on_exceeded": "continue" }),
    );
    let compiled = compile(&graph).expect("compile");
    let mut caps = mock_capabilities();
    caps.tools = Arc::new(Echo);
    let outcome = tokio::time::timeout(GUARD, tinyflows::engine::run(&compiled, json!({}), &caps))
        .await
        .expect("refinement loop hung")
        .expect("refinement loop runs");
    assert_eq!(gathered_candidates(&outcome.output), vec![20, 21]);
    assert_eq!(
        outcome.output["nodes"]["evaluate"]["lanes"]
            .as_object()
            .map(serde_json::Map::len),
        Some(3),
        "the stale pass-1 slot is still in state; only the gather ignores it"
    );
}
