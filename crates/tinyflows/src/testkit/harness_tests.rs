//! Tests for the test harness.
//!
//! These are tests of a testing tool, so each one asserts on what the harness
//! *reports* about a deliberately-shaped graph, rather than on the engine.

use super::*;
use crate::model::{Edge, Node, NodeKind, TriggerKind};
use serde_json::json;

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

fn edge(from_node: &str, to_node: &str) -> Edge {
    Edge {
        from_node: from_node.to_string(),
        from_port: "main".to_string(),
        to_node: to_node.to_string(),
        to_port: "main".to_string(),
    }
}

fn graph(call_config: Value) -> WorkflowGraph {
    WorkflowGraph {
        name: "harnessed".to_string(),
        nodes: vec![
            node(
                "t",
                NodeKind::Trigger,
                json!({ "kind": TriggerKind::Manual }),
            ),
            node("call", NodeKind::ToolCall, call_config),
        ],
        edges: vec![edge("t", "call")],
        ..Default::default()
    }
}

#[tokio::test]
async fn a_mocked_tool_answers_and_is_counted() {
    let run = TestHarness::new(&graph(json!({ "slug": "svc.do" })))
        .trigger(json!({ "q": "go" }))
        .mock_tool("svc.do", Respond::value(json!({ "ok": true })))
        .run()
        .await
        .expect("run");

    run.assert_completed();
    run.assert_node_ran("call");
    run.assert_call_count(super::super::capability::TOOLS, Some("svc.do"), 1);
    assert_eq!(run.node_output("call")[0]["json"], json!({ "ok": true }));
}

#[tokio::test]
async fn assert_no_null_bindings_passes_on_a_correctly_wired_graph() {
    let run = TestHarness::new(&graph(
        json!({ "slug": "svc.do", "args": { "q": "=nodes.t.item.q" } }),
    ))
    .trigger(json!({ "q": "go" }))
    .mock_tool("svc.do", Respond::value(json!({ "ok": true })))
    .run()
    .await
    .expect("run");

    run.assert_no_null_bindings();
}

#[tokio::test]
#[should_panic(expected = "resolved to null")]
async fn assert_no_null_bindings_catches_the_failure_a_green_run_hides() {
    // The run completes, nothing errors, and the workflow does nothing. This is
    // the case the whole assertion exists for.
    let run = TestHarness::new(&graph(
        json!({ "slug": "svc.do", "args": { "q": "=nodes.t.item.missing" } }),
    ))
    .trigger(json!({ "q": "go" }))
    .mock_tool("svc.do", Respond::value(json!({ "ok": true })))
    .run()
    .await
    .expect("the run itself succeeds — that is the problem");

    run.assert_completed();
    run.assert_no_null_bindings();
}

#[tokio::test]
async fn the_null_binding_message_names_the_upstream_node() {
    let run = TestHarness::new(&graph(
        json!({ "slug": "svc.do", "args": { "q": "=nodes.t.item.missing" } }),
    ))
    .trigger(json!({}))
    .run()
    .await
    .expect("run");

    let message = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run.assert_no_null_bindings()
    }))
    .expect_err("should panic");
    let message = message
        .downcast_ref::<String>()
        .expect("panic carries a message");
    assert!(message.contains("args.q"), "got {message}");
    assert!(
        message.contains("reads from node \"t\""),
        "the message should point at the upstream node, got {message}"
    );
}

#[tokio::test]
async fn a_sequenced_mock_drives_a_retry() {
    let run = TestHarness::new(&graph(json!({
        "slug": "svc.do",
        "retry": { "max_attempts": 2 }
    })))
    .mock_tool(
        "svc.do",
        Respond::sequence([
            Respond::error("transient"),
            Respond::value(json!({ "ok": true })),
        ]),
    )
    .run()
    .await
    .expect("the retry recovers");

    run.assert_completed();
    run.assert_call_count(super::super::capability::TOOLS, Some("svc.do"), 2);
    assert_eq!(run.node_output("call")[0]["json"], json!({ "ok": true }));
}

#[tokio::test]
async fn a_failing_node_is_reported_as_failed() {
    let run = TestHarness::new(&graph(json!({ "slug": "svc.do", "on_error": "continue" })))
        .mock_tool("svc.do", Respond::error("boom"))
        .run()
        .await
        .expect("`on_error: continue` completes the run");

    run.assert_node_failed("call");
}

#[tokio::test]
#[should_panic(expected = "never ran")]
async fn assert_node_ran_catches_a_node_that_did_not() {
    let run = TestHarness::new(&graph(json!({ "slug": "svc.do" })))
        .run()
        .await
        .expect("run");
    run.assert_node_ran("nonexistent");
}

#[tokio::test]
async fn a_per_node_mock_leaves_other_nodes_alone() {
    let graph = WorkflowGraph {
        name: "two_callers".to_string(),
        nodes: vec![
            node(
                "t",
                NodeKind::Trigger,
                json!({ "kind": TriggerKind::Manual }),
            ),
            node("a", NodeKind::ToolCall, json!({ "slug": "svc.do" })),
            node("b", NodeKind::ToolCall, json!({ "slug": "svc.do" })),
        ],
        edges: vec![edge("t", "a"), edge("a", "b")],
        ..Default::default()
    };

    let run = TestHarness::new(&graph)
        .mock_tool("svc.do", Respond::value(json!({ "stubbed": true })))
        .only_from("a")
        .run()
        .await
        .expect("run");

    assert_eq!(run.node_output("a")[0]["json"], json!({ "stubbed": true }));
    // `b` fell through to the default echo rather than the stub.
    assert_eq!(run.node_output("b")[0]["json"]["tool"], json!("svc.do"));
}

#[tokio::test]
async fn the_trace_is_available_for_anything_the_assertions_do_not_cover() {
    let run = TestHarness::new(&graph(json!({ "slug": "svc.do" })))
        .run()
        .await
        .expect("run");

    let trace = run.trace();
    assert_eq!(trace.steps.len(), 1, "one non-trigger node ran");
    assert!(
        trace.summary().contains("1 steps"),
        "got {}",
        trace.summary()
    );
}
