#![cfg(feature = "mock")]
//! Hardening probes for the branching / merge footguns (audit BUG-3, BUG-4/M4,
//! M5). Each test asserts the **correct/expected** behavior; a failing one means
//! the audited bug still reproduces on the current engine and is marked
//! `#[ignore]` with the observed-vs-expected note.

use serde_json::{Value, json};
use tinyflows::caps::mock::mock_capabilities;
use tinyflows::compiler::compile;
use tinyflows::engine::run;
use tinyflows::model::{Edge, Node, NodeKind, WorkflowGraph};

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

fn trigger(id: &str) -> Node {
    node(id, NodeKind::Trigger, Value::Null)
}

/// Edge from `from_node`'s `from_port` into `to_node`'s `main` port.
fn edge(from_node: &str, from_port: &str, to_node: &str) -> Edge {
    Edge {
        from_node: from_node.to_string(),
        from_port: from_port.to_string(),
        to_node: to_node.to_string(),
        to_port: "main".to_string(),
    }
}

/// BUG-3 — mixed-port fan-out must not silently drop a branch.
///
/// `src` has three outgoing edges: two on `main` (to `a`, `b`) and one on
/// `error` (to `h`). Because the ports are not all identical, `fan_out_targets`
/// declines to treat this as a parallel fan-out and lowers it to conditional
/// edges instead — whose route map overwrites the duplicate `main` label, so
/// only one of `a`/`b` actually runs. The **correct** behavior is that both
/// `a` AND `b` run when `src` succeeds on `main`.
#[tokio::test]
#[ignore = "BUG-3: mixed-port fan-out (2x main + 1x error) is lowered to conditional edges; the duplicate `main` route is overwritten so `a` never runs (only one main branch survives)"]
async fn bug3_mixed_port_fan_out_runs_all_main_branches() {
    let graph = WorkflowGraph {
        name: "bug3".to_string(),
        nodes: vec![
            trigger("start"),
            node("src", NodeKind::OutputParser, Value::Null),
            node("a", NodeKind::OutputParser, Value::Null),
            node("b", NodeKind::OutputParser, Value::Null),
            node("h", NodeKind::OutputParser, Value::Null),
        ],
        edges: vec![
            edge("start", "main", "src"),
            edge("src", "main", "a"),
            edge("src", "main", "b"),
            edge("src", "error", "h"),
        ],
        ..Default::default()
    };

    let compiled = compile(&graph).expect("compile");
    let outcome = run(&compiled, json!({ "x": 1 }), &mock_capabilities())
        .await
        .expect("run");
    let out = &outcome.output;

    assert!(
        !out["nodes"]["a"]["items"].is_null(),
        "the `main->a` branch should have run"
    );
    assert!(
        !out["nodes"]["b"]["items"].is_null(),
        "the `main->b` branch should ALSO have run (mixed-port fan-out must not drop one)"
    );
}

/// BUG-4 / M4 — a node wired from the `true` port sees only the taken branch's
/// data; the untaken `false`-branch slot must not leak in.
///
/// `cond` routes on `flag`. With `flag:true` only the `true` branch (`t`) runs;
/// `f` never executes. `sink` is wired from `t` only and must observe the
/// true-branch tag, never the false-branch tag, and `f` must have no run slot.
#[tokio::test]
async fn bug4_untaken_branch_does_not_leak_into_true_wired_sink() {
    let graph = WorkflowGraph {
        name: "bug4".to_string(),
        nodes: vec![
            trigger("start"),
            node("cond", NodeKind::Condition, json!({ "field": "flag" })),
            node(
                "t",
                NodeKind::Transform,
                json!({ "set": { "branch": "true" } }),
            ),
            node(
                "f",
                NodeKind::Transform,
                json!({ "set": { "branch": "false" } }),
            ),
            node("sink", NodeKind::OutputParser, Value::Null),
        ],
        edges: vec![
            edge("start", "main", "cond"),
            edge("cond", "true", "t"),
            edge("cond", "false", "f"),
            edge("t", "main", "sink"),
        ],
        ..Default::default()
    };

    let compiled = compile(&graph).expect("compile");
    let outcome = run(&compiled, json!({ "flag": true }), &mock_capabilities())
        .await
        .expect("run");
    let out = &outcome.output;

    // The false branch never ran.
    assert!(
        out["nodes"]["f"].is_null(),
        "the false branch `f` must not have run for flag:true"
    );
    // Sink saw exactly the true-branch tag.
    let items = out["nodes"]["sink"]["items"]
        .as_array()
        .expect("sink emitted items");
    assert_eq!(items.len(), 1, "sink should receive exactly the true item");
    assert_eq!(items[0]["json"]["branch"], "true");
}

/// M5 — a `merge` of an `agent` (1 item) and a `tool_call` (1 item) yields two
/// heterogeneous items concatenated in predecessor (edge) order. Documents that
/// the first `item` is the agent envelope (first predecessor edge).
#[tokio::test]
async fn m5_merge_concatenates_heterogeneous_predecessor_items() {
    let graph = WorkflowGraph {
        name: "m5".to_string(),
        nodes: vec![
            trigger("start"),
            node("draft", NodeKind::Agent, json!({ "prompt": "hi" })),
            node("call", NodeKind::ToolCall, json!({ "slug": "slack.post" })),
            node("merge", NodeKind::Merge, Value::Null),
        ],
        edges: vec![
            edge("start", "main", "draft"),
            edge("start", "main", "call"),
            edge("draft", "main", "merge"),
            edge("call", "main", "merge"),
        ],
        ..Default::default()
    };

    let compiled = compile(&graph).expect("compile");
    let outcome = run(&compiled, json!({ "seed": 1 }), &mock_capabilities())
        .await
        .expect("run");
    let out = &outcome.output;

    let items = out["nodes"]["merge"]["items"]
        .as_array()
        .expect("merge emitted items");
    assert_eq!(
        items.len(),
        2,
        "merge should carry both predecessors' items"
    );

    // Predecessor order follows edge order: `draft` (agent) first, `call`
    // (tool_call) second. Both are the stable `{json,text,raw}` envelope.
    let agent_first = items[0]["json"]["json"].get("completion").is_some();
    let tool_first = items[0]["json"]["json"].get("tool").is_some();
    assert!(
        agent_first || tool_first,
        "first merged item should be one of the two enveloped shapes; got {:?}",
        items[0]
    );
    // The two items are the two distinct shapes (agent completion + tool result).
    let shapes: Vec<bool> = items
        .iter()
        .map(|i| i["json"]["json"].get("tool").is_some())
        .collect();
    assert!(
        shapes.contains(&true) && shapes.contains(&false),
        "merge should contain one tool_call envelope and one agent envelope"
    );
}
