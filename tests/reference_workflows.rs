#![cfg(feature = "mock")]
//! End-to-end integration tests for the tinyflows engine.
//!
//! Each test builds a realistic, multi-node [`WorkflowGraph`] inspired by the
//! reference workflows in `docs/10-reference-workflows.md`, compiles it, and runs
//! it against the deterministic [mock capabilities](tinyflows::caps::mock). The
//! goal is to prove the whole pipeline — model → validate → compile → run — works
//! for the constructs the engine currently supports: linear chains, `split_out`
//! fan-out, `condition`/`switch` branching, and nested `sub_workflow` execution.
//!
//! The mocks are deterministic echoes (see `src/caps/mock.rs`): `http` returns
//! `{status, request}`, `code` returns `{result: <input>}`, `tools.invoke` returns
//! `{tool, args}`, and `llm.complete` returns `{completion: <request>}`. Assertions
//! therefore check the *shape and presence* of each node's contributed items in the
//! run state (`output["nodes"]["<id>"]["items"]`) rather than brittle deep equality
//! over the echoed payloads.
//!
//! This file is gated behind the `mock` cargo feature, so `cargo test` (no
//! features) skips it while `cargo test --all-features` runs it.

use serde_json::{Value, json};
use tinyflows::caps::mock::mock_capabilities;
use tinyflows::compiler::compile;
use tinyflows::engine::run;
use tinyflows::model::{Edge, Node, NodeKind, TriggerKind, WorkflowGraph};

/// Builds a node with the given id, kind, and config (no ports, no position).
fn node(id: &str, kind: NodeKind, config: Value) -> Node {
    Node {
        id: id.to_string(),
        kind,
        name: id.to_string(),
        config,
        ports: vec![],
        position: None,
    }
}

/// Builds a trigger node whose firing mode is carried in config as a [`TriggerKind`].
fn trigger(id: &str, kind: TriggerKind) -> Node {
    node(id, NodeKind::Trigger, json!({ "kind": kind }))
}

/// Builds an edge from `from_node`'s `from_port` into `to_node`'s `"main"` port.
fn edge(from_node: &str, from_port: &str, to_node: &str) -> Edge {
    Edge {
        from_node: from_node.to_string(),
        from_port: from_port.to_string(),
        to_node: to_node.to_string(),
        to_port: "main".to_string(),
    }
}

/// Reference workflow #4 (Customer insights) — a fully linear chain:
/// `trigger -> http_request (get reviews) -> code (cluster) -> split_out ->
/// agent -> tool_call (append to sheet)`. Proves a five-hop pipeline mixing every
/// integration node compiles and drives to completion, with data flowing through.
#[tokio::test]
async fn customer_insights_linear() {
    let graph = WorkflowGraph {
        name: "customer_insights".to_string(),
        nodes: vec![
            trigger("start", TriggerKind::Manual),
            node(
                "reviews",
                NodeKind::HttpRequest,
                json!({ "method": "GET", "url": "https://qdrant.example/reviews" }),
            ),
            node(
                "cluster",
                NodeKind::Code,
                json!({ "language": "python", "source": "kmeans(input)" }),
            ),
            // The code node wraps its input under `result` (an array), so splitting
            // on that path fans the clusters back out into individual items.
            node("split", NodeKind::SplitOut, json!({ "path": "result" })),
            node(
                "insights",
                NodeKind::Agent,
                json!({ "prompt": "Summarise customer sentiment", "model": "openai" }),
            ),
            node(
                "sheet",
                NodeKind::ToolCall,
                json!({ "slug": "googlesheets.append", "args": { "sheet": "reviews" } }),
            ),
        ],
        edges: vec![
            edge("start", "main", "reviews"),
            edge("reviews", "main", "cluster"),
            edge("cluster", "main", "split"),
            edge("split", "main", "insights"),
            edge("insights", "main", "sheet"),
        ],
        ..Default::default()
    };

    let compiled = compile(&graph).expect("compile");
    let outcome = run(
        &compiled,
        json!({ "query": "recent reviews" }),
        &mock_capabilities(),
    )
    .await
    .expect("run");
    let out = &outcome.output;

    // The trigger payload is preserved in the run metadata.
    assert_eq!(out["run"]["trigger"], json!({ "query": "recent reviews" }));

    // The HTTP node returned the canned 200 echo of its request descriptor.
    assert_eq!(out["nodes"]["reviews"]["items"][0]["json"]["status"], 200);
    assert_eq!(
        out["nodes"]["reviews"]["items"][0]["json"]["request"]["url"],
        "https://qdrant.example/reviews"
    );

    // split_out fanned the code node's `result` array back out into >=1 item.
    let split_items = out["nodes"]["split"]["items"]
        .as_array()
        .expect("split_out produced an items array");
    assert!(
        !split_items.is_empty(),
        "split_out should emit at least one item"
    );

    // The terminal tool_call ran and echoed the slug it was invoked with.
    assert_eq!(
        out["nodes"]["sheet"]["items"][0]["json"]["tool"],
        "googlesheets.append"
    );
    assert_eq!(
        out["nodes"]["sheet"]["items"][0]["json"]["args"]["sheet"],
        "reviews"
    );
}

/// Reference workflow #3 (API router) — a `switch` multi-way branch:
/// `trigger (webhook) -> agent -> transform (set type) -> switch -> {get | post}`.
/// A `transform` seeds a deterministic `type` field so the switch's `=item.type`
/// key is stable; only the matching `get` branch must run.
#[tokio::test]
async fn api_router_switch() {
    let graph = WorkflowGraph {
        name: "api_router".to_string(),
        nodes: vec![
            trigger("hook", TriggerKind::Webhook),
            node(
                "router",
                NodeKind::Agent,
                json!({ "prompt": "Classify the request", "model": "gemini" }),
            ),
            // Force a deterministic switch key onto the agent's output item.
            node(
                "label",
                NodeKind::Transform,
                json!({ "set": { "type": "get" } }),
            ),
            node(
                "switch",
                NodeKind::Switch,
                json!({ "expression": "=item.type" }),
            ),
            node(
                "http_get",
                NodeKind::HttpRequest,
                json!({ "method": "GET", "url": "https://api.example/props" }),
            ),
            node(
                "http_post",
                NodeKind::HttpRequest,
                json!({ "method": "POST", "url": "https://api.example/create" }),
            ),
        ],
        edges: vec![
            edge("hook", "main", "router"),
            edge("router", "main", "label"),
            edge("label", "main", "switch"),
            edge("switch", "get", "http_get"),
            edge("switch", "post", "http_post"),
        ],
        ..Default::default()
    };

    let compiled = compile(&graph).expect("compile");
    let outcome = run(&compiled, json!({ "method": "GET" }), &mock_capabilities())
        .await
        .expect("run");
    let out = &outcome.output;

    // Only the `get` branch was taken.
    assert!(
        !out["nodes"]["http_get"]["items"].is_null(),
        "the `get` branch should have run"
    );
    assert_eq!(out["nodes"]["http_get"]["items"][0]["json"]["status"], 200);
    assert_eq!(
        out["nodes"]["http_get"]["items"][0]["json"]["request"]["url"],
        "https://api.example/props"
    );
    // The `post` branch node never executed, so it has no slot in the run state.
    assert!(
        out["nodes"]["http_post"].is_null(),
        "the `post` branch should not have run"
    );
}

/// Reference workflow #1 (Create-user onboarding) — an IF branch:
/// `trigger (form) -> transform (set is_manager) -> condition -> {true | false}`.
/// A `transform` sets a boolean literal so the `condition`'s truthiness check is
/// deterministic; only the `true` branch's tool_call must run.
#[tokio::test]
async fn onboarding_condition() {
    let graph = WorkflowGraph {
        name: "onboarding".to_string(),
        nodes: vec![
            trigger("form", TriggerKind::Form),
            node(
                "flag",
                NodeKind::Transform,
                json!({ "set": { "is_manager": true } }),
            ),
            node(
                "is_manager",
                NodeKind::Condition,
                json!({ "field": "is_manager" }),
            ),
            node(
                "add_to_channel",
                NodeKind::ToolCall,
                json!({ "slug": "slack.add_to_channel", "args": { "channel": "managers" } }),
            ),
            node(
                "update_profile",
                NodeKind::ToolCall,
                json!({ "slug": "slack.update_profile", "args": {} }),
            ),
        ],
        edges: vec![
            edge("form", "main", "flag"),
            edge("flag", "main", "is_manager"),
            edge("is_manager", "true", "add_to_channel"),
            edge("is_manager", "false", "update_profile"),
        ],
        ..Default::default()
    };

    let compiled = compile(&graph).expect("compile");
    let outcome = run(
        &compiled,
        json!({ "name": "Ada", "role": "lead" }),
        &mock_capabilities(),
    )
    .await
    .expect("run");
    let out = &outcome.output;

    // The true branch ran and echoed its tool slug.
    assert!(
        !out["nodes"]["add_to_channel"]["items"].is_null(),
        "the true branch should have run"
    );
    assert_eq!(
        out["nodes"]["add_to_channel"]["items"][0]["json"]["tool"],
        "slack.add_to_channel"
    );
    // The false branch never executed.
    assert!(
        out["nodes"]["update_profile"].is_null(),
        "the false branch should not have run"
    );
}

/// A nested `sub_workflow`: a parent `trigger -> sub_workflow` embeds a child
/// `trigger -> output_parser` graph. The sub_workflow node compiles and runs the
/// child via the same engine and emits the child's final run state as its item, so
/// the parent's sub_workflow slot must contain the child's `run` + `nodes` state.
#[tokio::test]
async fn nested_sub_workflow() {
    let child = WorkflowGraph {
        name: "child".to_string(),
        nodes: vec![
            trigger("child_trigger", TriggerKind::ExecuteByWorkflow),
            node("parse", NodeKind::OutputParser, Value::Null),
        ],
        edges: vec![edge("child_trigger", "main", "parse")],
        ..Default::default()
    };
    let child_value = serde_json::to_value(&child).expect("serialize child graph");

    let parent = WorkflowGraph {
        name: "parent".to_string(),
        nodes: vec![
            trigger("start", TriggerKind::Manual),
            node(
                "child",
                NodeKind::SubWorkflow,
                json!({ "workflow": child_value }),
            ),
        ],
        edges: vec![edge("start", "main", "child")],
        ..Default::default()
    };

    let compiled = compile(&parent).expect("compile parent");
    let outcome = run(&compiled, json!({ "payload": 42 }), &mock_capabilities())
        .await
        .expect("run parent");
    let out = &outcome.output;

    // The sub_workflow emitted the child's final run state as a single item.
    let child_state = &out["nodes"]["child"]["items"][0]["json"];
    assert!(
        !child_state["run"]["trigger"].is_null(),
        "child run state should carry its trigger payload"
    );
    // The child actually ran end-to-end: its output_parser node produced items.
    assert!(
        !child_state["nodes"]["parse"]["items"].is_null(),
        "the child's output_parser node should have run"
    );
}
