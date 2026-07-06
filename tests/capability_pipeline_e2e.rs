#![cfg(feature = "mock")]
//! End-to-end integration tests for a full capability-backed pipeline.
//!
//! These drive a linear chain that touches every host capability in turn —
//! `http_request` (HttpClient), `code` (CodeRunner), `agent` (LlmProvider),
//! `tool_call` (ToolInvoker) — terminated by an `output_parser` passthrough,
//! against the deterministic [mock capabilities](tinyflows::caps::mock).
//!
//! Because the integration nodes echo deterministically (http -> `{status,
//! request, connection}`, code -> `{result}`, llm -> `{completion, connection}`,
//! tools -> `{tool, args, connection}`) and `output_parser` passes its input
//! through unchanged, the terminal node's item is the upstream `tool_call`'s
//! echo. We assert on that, on `connection_ref` threading, and on the
//! missing-`slug` failure surfacing out of the whole run.
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

/// Builds an edge from `from_node` into `to_node` on the `"main"` ports.
fn edge(from_node: &str, to_node: &str) -> Edge {
    Edge {
        from_node: from_node.to_string(),
        from_port: "main".to_string(),
        to_node: to_node.to_string(),
        to_port: "main".to_string(),
    }
}

/// Builds the linear pipeline `trigger -> http -> code -> agent -> tool -> parse`,
/// letting the caller supply the `tool_call`'s config so tests can vary the slug
/// and connection.
fn pipeline(tool_config: Value) -> WorkflowGraph {
    WorkflowGraph {
        name: "cap_pipeline".to_string(),
        nodes: vec![
            trigger("start"),
            node(
                "fetch",
                NodeKind::HttpRequest,
                json!({ "method": "GET", "url": "https://svc/data" }),
            ),
            node(
                "transform_code",
                NodeKind::Code,
                json!({ "language": "python", "source": "shape(input)" }),
            ),
            node(
                "reason",
                NodeKind::Agent,
                json!({ "prompt": "summarise", "model": "any" }),
            ),
            node("act", NodeKind::ToolCall, tool_config),
            node("parse", NodeKind::OutputParser, Value::Null),
        ],
        edges: vec![
            edge("start", "fetch"),
            edge("fetch", "transform_code"),
            edge("transform_code", "reason"),
            edge("reason", "act"),
            edge("act", "parse"),
        ],
        ..Default::default()
    }
}

#[tokio::test]
async fn linear_pipeline_threads_data_through_every_capability() {
    let graph = pipeline(json!({ "slug": "sheets.append", "args": { "row": 1 } }));
    let compiled = compile(&graph).expect("compile");
    let outcome = run(&compiled, json!({ "q": "go" }), &mock_capabilities())
        .await
        .expect("run");
    let out = &outcome.output;

    // Every capability node produced its deterministic echo.
    assert_eq!(
        out["nodes"]["fetch"]["items"][0]["json"]["json"]["status"],
        200
    );
    assert!(
        !out["nodes"]["transform_code"]["items"][0]["json"]["result"].is_null(),
        "code node should wrap its input under `result`"
    );
    assert_eq!(
        out["nodes"]["reason"]["items"][0]["json"]["json"]["completion"]["prompt"], "summarise",
        "agent output is enveloped: the completion is under json.completion"
    );

    // The terminal output_parser echoes the upstream tool_call's item unchanged.
    assert_eq!(
        out["nodes"]["parse"]["items"][0]["json"]["json"]["tool"], "sheets.append",
        "the final node should carry the tool_call's echoed slug"
    );
    assert_eq!(
        out["nodes"]["parse"]["items"][0]["json"]["json"]["args"]["row"], 1,
        "the final node should carry the tool_call's echoed args"
    );
}

#[tokio::test]
async fn pipeline_threads_connection_ref_to_the_mock() {
    // A `connection_ref` on the tool_call is threaded to ToolInvoker::invoke, and
    // the mock echoes it back under `connection`; the passthrough parser surfaces
    // it at the end of the pipeline.
    let graph = pipeline(json!({
        "slug": "sheets.append",
        "connection_ref": "composio:sheets:acct_42",
    }));
    let compiled = compile(&graph).expect("compile");
    let outcome = run(&compiled, json!({ "q": "go" }), &mock_capabilities())
        .await
        .expect("run");
    let out = &outcome.output;

    // The tool node saw the connection ...
    assert_eq!(
        out["nodes"]["act"]["items"][0]["json"]["json"]["connection"],
        "composio:sheets:acct_42"
    );
    // ... and it survives through the terminal output_parser.
    assert_eq!(
        out["nodes"]["parse"]["items"][0]["json"]["json"]["connection"], "composio:sheets:acct_42",
        "the threaded connection_ref should reach the end of the pipeline"
    );
}

#[tokio::test]
async fn pipeline_missing_tool_slug_surfaces_as_run_error() {
    // The tool_call has no `slug` and the default `stop` policy, so its
    // Capability error must fail the whole pipeline run.
    let graph = pipeline(json!({ "args": { "row": 1 } }));
    let compiled = compile(&graph).expect("compile");
    let result = run(&compiled, json!({ "q": "go" }), &mock_capabilities()).await;

    assert!(
        result.is_err(),
        "a tool_call with no slug must surface an error out of the run, got {result:?}"
    );
}
