#![cfg(feature = "mock")]
//! End-to-end integration tests for per-node error handling.
//!
//! A node's error policy lives in its free-form `config` (see `src/engine.rs`):
//! `on_error` selects what happens once retries are exhausted — `"continue"`
//! turns the failure into an error item on the main port, `"route"` emits it on
//! the `error` port so the graph can route it to a recovery sub-graph, and
//! `"stop"` (the default, and any unknown value) fails the whole run. A
//! `retry.max_attempts` bounds the number of executor attempts.
//!
//! We use a `tool_call` node with **no `slug`** as the deterministic failure: its
//! executor returns a `Capability` error every time.
//!
//! The error item shape is `{ "error": { "message", "node" } }`.
//!
//! Gated behind the `mock` cargo feature.

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
        type_version: 1,
        name: id.to_string(),
        config,
        ports: vec![],
        position: None,
    }
}

/// Builds a manual trigger node.
fn trigger(id: &str) -> Node {
    node(
        id,
        NodeKind::Trigger,
        json!({ "kind": TriggerKind::Manual }),
    )
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

#[tokio::test]
async fn on_error_continue_emits_error_item_on_main() {
    // trigger -> failing tool_call(on_error=continue). The run must complete Ok
    // and the failing node's main-port item must be the structured error item.
    let graph = WorkflowGraph {
        name: "err_continue".to_string(),
        nodes: vec![
            trigger("start"),
            node(
                "boom",
                NodeKind::ToolCall,
                json!({ "on_error": "continue" }),
            ),
        ],
        edges: vec![edge("start", "main", "boom")],
        ..Default::default()
    };

    let compiled = compile(&graph).expect("compile");
    let outcome = run(&compiled, json!({}), &mock_capabilities())
        .await
        .expect("on_error=continue should not fail the run");
    let out = &outcome.output;

    assert_eq!(
        out["nodes"]["boom"]["items"][0]["json"]["error"]["node"], "boom",
        "the error item should name the failing node"
    );
    assert!(
        out["nodes"]["boom"]["items"][0]["json"]["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("slug")),
        "the error message should mention the missing slug"
    );
}

#[tokio::test]
async fn on_error_route_delivers_error_item_to_recovery_node() {
    // trigger -> failing tool_call(on_error=route) --error--> recovery. The error
    // item must reach the recovery node via the `error` port.
    let graph = WorkflowGraph {
        name: "err_route".to_string(),
        nodes: vec![
            trigger("start"),
            node("boom", NodeKind::ToolCall, json!({ "on_error": "route" })),
            node("recover", NodeKind::OutputParser, Value::Null),
        ],
        edges: vec![
            edge("start", "main", "boom"),
            edge("boom", "error", "recover"),
        ],
        ..Default::default()
    };

    let compiled = compile(&graph).expect("compile");
    let outcome = run(&compiled, json!({}), &mock_capabilities())
        .await
        .expect("on_error=route should not fail the run");
    let out = &outcome.output;

    // The failing node emitted on the `error` port ...
    assert_eq!(out["nodes"]["boom"]["port"], "error");
    // ... and the recovery node received (and passed through) the error item.
    assert_eq!(
        out["nodes"]["recover"]["items"][0]["json"]["error"]["node"], "boom",
        "the recovery node should receive the routed error item"
    );
}

#[tokio::test]
async fn on_error_stop_fails_the_whole_run() {
    // trigger -> failing tool_call with the default `stop` policy (no on_error).
    // The run must surface an Err rather than completing.
    let graph = WorkflowGraph {
        name: "err_stop".to_string(),
        nodes: vec![
            trigger("start"),
            node("boom", NodeKind::ToolCall, Value::Null),
        ],
        edges: vec![edge("start", "main", "boom")],
        ..Default::default()
    };

    let compiled = compile(&graph).expect("compile");
    let result = run(&compiled, json!({}), &mock_capabilities()).await;

    assert!(
        result.is_err(),
        "the default stop policy must fail the whole run, got {result:?}"
    );
}

#[tokio::test]
async fn retry_max_attempts_then_continue_completes() {
    // A deterministically-failing tool with `retry.max_attempts = 3` and
    // `on_error = continue`: after exhausting the attempts it yields an error
    // item and the run completes.
    let graph = WorkflowGraph {
        name: "err_retry".to_string(),
        nodes: vec![
            trigger("start"),
            node(
                "boom",
                NodeKind::ToolCall,
                json!({ "retry": { "max_attempts": 3 }, "on_error": "continue" }),
            ),
        ],
        edges: vec![edge("start", "main", "boom")],
        ..Default::default()
    };

    let compiled = compile(&graph).expect("compile");
    let outcome = run(&compiled, json!({}), &mock_capabilities())
        .await
        .expect("retry-then-continue should complete");
    let out = &outcome.output;

    assert_eq!(
        out["nodes"]["boom"]["items"][0]["json"]["error"]["node"], "boom",
        "after exhausting retries, continue yields the error item"
    );
}
