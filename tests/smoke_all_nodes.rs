#![cfg(feature = "mock")]
//! Smoke coverage for every node kind that can run against the mock capabilities.
//!
//! For each kind (`agent`, `tool_call`, `http_request`, `code`, `output_parser`,
//! `sub_workflow`, `condition`, `switch`, `merge`, `split_out`, `transform`) this
//! builds a minimal `trigger -> <node>` workflow with valid config, runs it, and
//! asserts the run succeeds and the node produced a well-formed slot (an `items`
//! array). A final big test chains many kinds together end to end.
//!
//! Gated behind the `mock` feature, so plain `cargo test` skips it while
//! `cargo test --all-features` runs it.

use serde_json::{Value, json};
use tinyflows::caps::mock::mock_capabilities;
use tinyflows::compiler::compile;
use tinyflows::engine::run;
use tinyflows::model::{Edge, Node, NodeKind, TriggerKind, WorkflowGraph};

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

/// Builds an edge from `from_node`'s `from_port` into `to_node`'s `main` port.
fn edge(from_node: &str, from_port: &str, to_node: &str) -> Edge {
    Edge {
        from_node: from_node.to_string(),
        from_port: from_port.to_string(),
        to_node: to_node.to_string(),
        to_port: "main".to_string(),
    }
}

/// Runs a minimal `trigger -> node("n", ...)` workflow with the given trigger
/// input and asserts the node produced a well-formed, non-empty `items` slot.
async fn smoke_single_node(kind: NodeKind, config: Value, input: Value) {
    let graph = WorkflowGraph {
        name: "smoke".to_string(),
        nodes: vec![trigger("t", TriggerKind::Manual), node("n", kind, config)],
        edges: vec![edge("t", "main", "n")],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let outcome = run(&compiled, input, &mock_capabilities())
        .await
        .expect("run should succeed");

    let items = outcome.output["nodes"]["n"]["items"]
        .as_array()
        .expect("the node should produce an items array");
    assert!(
        !items.is_empty(),
        "the node should emit at least one item, got an empty slot"
    );
    // Each emitted item is a well-formed Item with a `json` payload.
    for item in items {
        assert!(
            item.get("json").is_some(),
            "each emitted item should carry a `json` field, got: {item}"
        );
    }
    assert!(
        outcome.pending_approvals.is_empty(),
        "a smoke run should complete with no pending approvals"
    );
}

#[tokio::test]
async fn smoke_agent() {
    smoke_single_node(NodeKind::Agent, json!({ "prompt": "hello" }), json!({})).await;
}

#[tokio::test]
async fn smoke_tool_call() {
    smoke_single_node(
        NodeKind::ToolCall,
        json!({ "slug": "slack.post", "args": { "channel": "ops" } }),
        json!({}),
    )
    .await;
}

#[tokio::test]
async fn smoke_http_request() {
    smoke_single_node(
        NodeKind::HttpRequest,
        json!({ "method": "GET", "url": "https://example.com/data" }),
        json!({}),
    )
    .await;
}

#[tokio::test]
async fn smoke_code() {
    smoke_single_node(
        NodeKind::Code,
        json!({ "language": "javascript", "source": "return items;" }),
        json!({ "seed": 1 }),
    )
    .await;
}

#[tokio::test]
async fn smoke_output_parser() {
    smoke_single_node(NodeKind::OutputParser, Value::Null, json!({ "x": 1 })).await;
}

#[tokio::test]
async fn smoke_sub_workflow() {
    // A minimal child graph (a single trigger) embedded in the node's config.
    let child = WorkflowGraph {
        name: "child".to_string(),
        nodes: vec![trigger("child_start", TriggerKind::ExecuteByWorkflow)],
        ..Default::default()
    };
    let child_value = serde_json::to_value(&child).expect("serialize child");
    smoke_single_node(
        NodeKind::SubWorkflow,
        json!({ "workflow": child_value }),
        json!({ "payload": 7 }),
    )
    .await;
}

#[tokio::test]
async fn smoke_condition() {
    // As a leaf node the condition still runs and records its routed items.
    smoke_single_node(
        NodeKind::Condition,
        json!({ "field": "active" }),
        json!({ "active": true }),
    )
    .await;
}

#[tokio::test]
async fn smoke_switch() {
    smoke_single_node(
        NodeKind::Switch,
        json!({ "field": "kind" }),
        json!({ "kind": "a" }),
    )
    .await;
}

#[tokio::test]
async fn smoke_merge() {
    // A single-predecessor merge is a passthrough of the trigger's items.
    smoke_single_node(NodeKind::Merge, Value::Null, json!({ "a": 1 })).await;
}

#[tokio::test]
async fn smoke_split_out() {
    smoke_single_node(
        NodeKind::SplitOut,
        json!({ "path": "items" }),
        json!({ "items": [1, 2, 3] }),
    )
    .await;
}

#[tokio::test]
async fn smoke_transform() {
    smoke_single_node(
        NodeKind::Transform,
        json!({ "set": { "tag": "smoke" } }),
        json!({ "x": 1 }),
    )
    .await;
}

/// One big workflow chaining many kinds together end to end:
/// `trigger -> http_request -> code -> split_out -> transform -> condition
/// -> {true: tool_call, false: output_parser}`, with the `true` branch feeding a
/// `merge` and then a final `agent`. Proves a heterogeneous pipeline drives to
/// completion with data flowing through.
#[tokio::test]
async fn big_chained_workflow_runs_end_to_end() {
    let graph = WorkflowGraph {
        name: "big_chain".to_string(),
        nodes: vec![
            trigger("start", TriggerKind::Manual),
            node(
                "fetch",
                NodeKind::HttpRequest,
                json!({ "method": "GET", "url": "https://api.example/items" }),
            ),
            node(
                "process",
                NodeKind::Code,
                json!({ "language": "python", "source": "transform(items)" }),
            ),
            // The code mock wraps its input under `result` (an array), so splitting
            // on that path fans it back out into per-element items.
            node("fan", NodeKind::SplitOut, json!({ "path": "result" })),
            node(
                "label",
                NodeKind::Transform,
                json!({ "set": { "stage": "labeled", "ok": true } }),
            ),
            node("check", NodeKind::Condition, json!({ "field": "ok" })),
            node(
                "notify",
                NodeKind::ToolCall,
                json!({ "slug": "slack.post", "args": { "channel": "ops" } }),
            ),
            node("skipped", NodeKind::OutputParser, Value::Null),
            node("gather", NodeKind::Merge, Value::Null),
            node(
                "summarize",
                NodeKind::Agent,
                json!({ "prompt": "Summarize the run" }),
            ),
        ],
        edges: vec![
            edge("start", "main", "fetch"),
            edge("fetch", "main", "process"),
            edge("process", "main", "fan"),
            edge("fan", "main", "label"),
            edge("label", "main", "check"),
            edge("check", "true", "notify"),
            edge("check", "false", "skipped"),
            edge("notify", "main", "gather"),
            edge("gather", "main", "summarize"),
        ],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let outcome = run(
        &compiled,
        json!({ "query": "recent" }),
        &mock_capabilities(),
    )
    .await
    .expect("run");
    let out = &outcome.output;

    // HTTP hop echoed the canned 200 response.
    assert_eq!(out["nodes"]["fetch"]["items"][0]["json"]["status"], 200);
    // split_out fanned the code node's `result` array into >= 1 item.
    assert!(
        !out["nodes"]["fan"]["items"]
            .as_array()
            .expect("split_out items")
            .is_empty(),
        "split_out should emit at least one item"
    );
    // The transform stamped its fields onto the fanned items.
    assert_eq!(
        out["nodes"]["label"]["items"][0]["json"]["stage"],
        json!("labeled")
    );
    // The condition took the `true` branch, so the tool_call ran and the `false`
    // branch's output_parser did not.
    assert_eq!(
        out["nodes"]["notify"]["items"][0]["json"]["tool"],
        json!("slack.post")
    );
    assert!(
        out["nodes"]["skipped"].is_null(),
        "the false branch should not have run"
    );
    // The merge barrier and the terminal agent both ran.
    assert!(
        !out["nodes"]["gather"]["items"].is_null(),
        "the merge should have run"
    );
    assert_eq!(
        out["nodes"]["summarize"]["items"][0]["json"]["completion"]["prompt"],
        json!("Summarize the run"),
        "the terminal agent should have completed its config request"
    );
    assert!(outcome.pending_approvals.is_empty());
}
