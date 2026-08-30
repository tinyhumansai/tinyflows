//! Tests for the tool dispatcher.
//!
//! Every one of these drives the registry the way an agent does — a tool name
//! and a JSON blob in, JSON out — and asserts only on that JSON. Nothing here
//! reaches for a Rust type the agent could not see, because the point is that
//! the JSON surface is sufficient on its own.

use super::*;
use serde_json::{Value, json};

/// A trigger, a tool call binding to a field nothing produces, and a transform.
///
/// Deliberately broken in the way that matters: the run completes, nothing
/// errors, and the workflow does nothing.
fn graph() -> Value {
    json!({
        "name": "agent_debugged",
        "nodes": [
            { "id": "t", "kind": "trigger", "name": "t", "config": { "kind": "manual" } },
            {
                "id": "call",
                "kind": "tool_call",
                "name": "call",
                "config": { "slug": "svc.do", "args": { "to": "=nodes.t.item.missing" } }
            },
            {
                "id": "after",
                "kind": "transform",
                "name": "after",
                "config": { "set": { "seen": "=item" } }
            }
        ],
        "edges": [
            { "from_node": "t", "from_port": "main", "to_node": "call", "to_port": "main" },
            { "from_node": "call", "from_port": "main", "to_node": "after", "to_port": "main" }
        ]
    })
}

#[tokio::test]
async fn an_unknown_tool_is_refused_with_a_stable_code() {
    let registry = TestkitRegistry::new();
    let err = registry
        .dispatch("flow_test.nope", json!({}))
        .await
        .expect_err("unknown tools are refused");
    assert_eq!(err.code, ToolErrorCode::UnknownTool);
}

#[tokio::test]
async fn a_missing_argument_is_refused_with_a_stable_code() {
    let registry = TestkitRegistry::new();
    let err = registry
        .dispatch("flow_test.run", json!({}))
        .await
        .expect_err("a run needs a graph");
    assert_eq!(err.code, ToolErrorCode::InvalidArguments);
}

#[tokio::test]
async fn a_malformed_graph_is_refused_rather_than_panicking() {
    let registry = TestkitRegistry::new();
    let err = registry
        .dispatch(
            "flow_test.run",
            json!({ "graph": { "nodes": "not a list" } }),
        )
        .await
        .expect_err("a malformed graph is refused");
    assert_eq!(err.code, ToolErrorCode::InvalidGraph);
}

#[tokio::test]
async fn running_reports_the_null_binding_a_green_run_hides() {
    // The headline case: the run completes, and the reply still says which
    // binding was empty and which node it was reading from.
    let registry = TestkitRegistry::new();
    let result = registry
        .dispatch("flow_test.run", json!({ "graph": graph() }))
        .await
        .expect("the run itself succeeds");

    assert_eq!(result["status"], "completed");
    let nulls = result["nullBindings"].as_array().expect("nullBindings");
    assert_eq!(nulls.len(), 1);
    assert_eq!(nulls[0]["nodeId"], "call");
    assert_eq!(nulls[0]["location"], "args.to");
    assert_eq!(
        nulls[0]["readsFrom"], "t",
        "the reply must point at the upstream node"
    );
}

#[tokio::test]
async fn a_programmed_mock_answers_the_run() {
    let registry = TestkitRegistry::new();
    let result = registry
        .dispatch(
            "flow_test.run",
            json!({
                "graph": graph(),
                "mocks": [
                    { "capability": "tools", "target": "svc.do", "value": { "ok": true } }
                ]
            }),
        )
        .await
        .expect("run");

    let run_id = result["runId"].as_str().expect("runId");
    let node = registry
        .dispatch(
            "flow_test.node",
            json!({ "run_id": run_id, "node_id": "call" }),
        )
        .await
        .expect("node");

    assert_eq!(node["ran"], true);
    assert_eq!(node["calls"][0]["target"], "svc.do");
    assert_eq!(
        node["activations"][0]["output"][0]["json"]["json"],
        json!({ "ok": true })
    );
}

#[tokio::test]
async fn a_sequenced_mock_is_programmable_over_json() {
    let registry = TestkitRegistry::new();
    let result = registry
        .dispatch(
            "flow_test.run",
            json!({
                "graph": {
                    "name": "retrying",
                    "nodes": [
                        { "id": "t", "kind": "trigger", "name": "t", "config": { "kind": "manual" } },
                        {
                            "id": "call", "kind": "tool_call", "name": "call",
                            "config": { "slug": "svc.do", "retry": { "max_attempts": 2 } }
                        }
                    ],
                    "edges": [
                        { "from_node": "t", "from_port": "main", "to_node": "call", "to_port": "main" }
                    ]
                },
                "mocks": [{
                    "capability": "tools",
                    "target": "svc.do",
                    "sequence": [{ "error": "transient" }, { "value": { "ok": true } }]
                }]
            }),
        )
        .await
        .expect("the retry recovers");

    assert_eq!(result["status"], "completed");
    assert!(
        result["summary"]
            .as_str()
            .expect("summary")
            .contains("0 failed"),
        "a recovered retry is not a failure: {}",
        result["summary"]
    );
}

#[tokio::test]
async fn a_trace_can_be_fetched_and_narrowed_to_one_node() {
    let registry = TestkitRegistry::new();
    let run = registry
        .dispatch("flow_test.run", json!({ "graph": graph() }))
        .await
        .expect("run");
    let run_id = run["runId"].as_str().expect("runId");

    let full = registry
        .dispatch("flow_test.trace", json!({ "run_id": run_id }))
        .await
        .expect("trace");
    assert_eq!(full["steps"].as_array().expect("steps").len(), 2);

    let narrowed = registry
        .dispatch(
            "flow_test.trace",
            json!({ "run_id": run_id, "node_id": "call" }),
        )
        .await
        .expect("trace");
    assert_eq!(narrowed["steps"].as_array().expect("steps").len(), 1);
}

#[tokio::test]
async fn an_unknown_run_is_refused() {
    let registry = TestkitRegistry::new();
    let err = registry
        .dispatch("flow_test.trace", json!({ "run_id": "nope" }))
        .await
        .expect_err("unknown runs are refused");
    assert_eq!(err.code, ToolErrorCode::UnknownRun);
}

#[tokio::test]
async fn asking_about_a_node_that_never_ran_says_so_rather_than_erroring() {
    let registry = TestkitRegistry::new();
    let run = registry
        .dispatch("flow_test.run", json!({ "graph": graph() }))
        .await
        .expect("run");
    let run_id = run["runId"].as_str().expect("runId");

    let node = registry
        .dispatch(
            "flow_test.node",
            json!({ "run_id": run_id, "node_id": "ghost" }),
        )
        .await
        .expect("a node that never ran is an answer, not an error");
    assert_eq!(node["ran"], false);
}

/// The whole point of the tool surface: an agent pauses a run, looks at it,
/// overrides a value, and steps on — entirely over JSON, across separate calls.
#[tokio::test]
async fn a_full_debug_session_can_be_driven_entirely_over_json() {
    let registry = TestkitRegistry::new();

    let started = registry
        .dispatch("flow_debug.start", json!({ "graph": graph() }))
        .await
        .expect("start");
    let session_id = started["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();

    let bp = registry
        .dispatch(
            "flow_debug.breakpoint",
            json!({ "session_id": session_id, "node_id": "call", "before": true }),
        )
        .await
        .expect("breakpoint");
    assert!(bp["breakpointId"].is_number());

    let waited = registry
        .dispatch(
            "flow_debug.wait",
            json!({ "session_id": session_id, "timeout_ms": 10_000 }),
        )
        .await
        .expect("wait");
    assert_eq!(waited["paused"], true);
    let pause = &waited["pause"];
    assert_eq!(pause["nodeId"], "call");
    assert_eq!(pause["phase"], "before");
    // The inspection an agent actually needs: which binding is about to be empty.
    assert_eq!(pause["nullBindings"][0][0], "args.to");

    let status = registry
        .dispatch("flow_debug.status", json!({ "session_id": session_id }))
        .await
        .expect("status");
    assert_eq!(status["status"], "paused");

    let pause_id = pause["pauseId"].as_u64().expect("pauseId");
    registry
        .dispatch(
            "flow_debug.release",
            json!({
                "session_id": session_id,
                "pause_id": pause_id,
                "command": "override",
                "items": [{ "fixed": true }]
            }),
        )
        .await
        .expect("release");

    let stopped = registry
        .dispatch("flow_debug.stop", json!({ "session_id": session_id }))
        .await
        .expect("stop");
    assert_eq!(stopped["stopped"], true);
    assert_eq!(
        stopped["output"]["nodes"]["after"]["items"][0]["json"]["seen"],
        json!({ "fixed": true }),
        "the override an agent supplied over JSON should have reached downstream"
    );
}

#[tokio::test]
async fn breakpoints_can_be_listed_and_cleared_over_json() {
    let registry = TestkitRegistry::new();
    let started = registry
        .dispatch("flow_debug.start", json!({ "graph": graph() }))
        .await
        .expect("start");
    let session_id = started["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();

    let bp = registry
        .dispatch(
            "flow_debug.breakpoint",
            json!({ "session_id": session_id, "node_id": "call" }),
        )
        .await
        .expect("set");
    let id = bp["breakpointId"].as_u64().expect("id");

    let listed = registry
        .dispatch(
            "flow_debug.breakpoint",
            json!({ "session_id": session_id, "action": "list" }),
        )
        .await
        .expect("list");
    assert_eq!(listed["breakpoints"].as_array().expect("array").len(), 1);

    let cleared = registry
        .dispatch(
            "flow_debug.breakpoint",
            json!({ "session_id": session_id, "action": "clear", "breakpoint_id": id }),
        )
        .await
        .expect("clear");
    assert_eq!(cleared["cleared"], true);

    let _ = registry
        .dispatch("flow_debug.stop", json!({ "session_id": session_id }))
        .await;
}

#[tokio::test]
async fn an_on_error_breakpoint_defaults_to_the_after_phase() {
    // An agent asking to "break where it fails" should not also have to know
    // that a failure only exists after the node has run.
    let registry = TestkitRegistry::new();
    let broken = json!({
        "name": "failing",
        "nodes": [
            { "id": "t", "kind": "trigger", "name": "t", "config": { "kind": "manual" } },
            { "id": "call", "kind": "tool_call", "name": "call", "config": { "on_error": "continue" } }
        ],
        "edges": [
            { "from_node": "t", "from_port": "main", "to_node": "call", "to_port": "main" }
        ]
    });
    let started = registry
        .dispatch("flow_debug.start", json!({ "graph": broken }))
        .await
        .expect("start");
    let session_id = started["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();

    registry
        .dispatch(
            "flow_debug.breakpoint",
            json!({ "session_id": session_id, "any": true, "on_error": true }),
        )
        .await
        .expect("breakpoint");

    let waited = registry
        .dispatch(
            "flow_debug.wait",
            json!({ "session_id": session_id, "timeout_ms": 10_000 }),
        )
        .await
        .expect("wait");
    assert_eq!(waited["paused"], true);
    assert_eq!(waited["pause"]["phase"], "after");
    assert!(waited["pause"]["error"].is_string());

    let _ = registry
        .dispatch("flow_debug.stop", json!({ "session_id": session_id }))
        .await;
}

#[tokio::test]
async fn an_unknown_session_is_refused() {
    let registry = TestkitRegistry::new();
    let err = registry
        .dispatch("flow_debug.status", json!({ "session_id": "nope" }))
        .await
        .expect_err("unknown sessions are refused");
    assert_eq!(err.code, ToolErrorCode::UnknownSession);
}

#[tokio::test]
async fn releasing_an_unknown_pause_is_refused() {
    let registry = TestkitRegistry::new();
    let started = registry
        .dispatch("flow_debug.start", json!({ "graph": graph() }))
        .await
        .expect("start");
    let session_id = started["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();

    let err = registry
        .dispatch(
            "flow_debug.release",
            json!({ "session_id": session_id, "pause_id": 99, "command": "continue" }),
        )
        .await
        .expect_err("an unknown pause is refused");
    assert_eq!(err.code, ToolErrorCode::UnknownPause);

    let _ = registry
        .dispatch("flow_debug.stop", json!({ "session_id": session_id }))
        .await;
}

#[tokio::test]
async fn waiting_on_a_run_with_no_breakpoints_reports_not_paused() {
    // A normal outcome, not an error: a breakpoint on a node a branch routed
    // past never fires either.
    let registry = TestkitRegistry::new();
    let started = registry
        .dispatch("flow_debug.start", json!({ "graph": graph() }))
        .await
        .expect("start");
    let session_id = started["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string();

    let waited = registry
        .dispatch(
            "flow_debug.wait",
            json!({ "session_id": session_id, "timeout_ms": 300 }),
        )
        .await
        .expect("wait");
    assert_eq!(waited["paused"], false);

    let _ = registry
        .dispatch("flow_debug.stop", json!({ "session_id": session_id }))
        .await;
}
