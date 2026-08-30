#![cfg(feature = "mock")]
//! End-to-end tests for **configurable agents**: a graph that declares its own
//! agent types, an `agent` node that narrows one, and the harness seam that
//! receives the assembled request.
//!
//! These drive the real engine rather than the executor directly, so they cover
//! the parts a unit test cannot: that the graph's top-level `agents` registry is
//! actually threaded into node execution, that a JSON-authored workflow round
//! trips through `serde` into a working run, and that the item envelope a
//! downstream node reads carries the agent's stop reason.

use serde_json::{Value, json};
use tinyflows::caps::mock::{
    MockAgentHarness, MockAgentRunner, mock_capabilities, mock_capabilities_with_agent,
};
use tinyflows::compiler::compile;
use tinyflows::engine::{RunInput, run};
use tinyflows::model::WorkflowGraph;

/// The reference workflow: a graph-declared agent type, narrowed by the node
/// that uses it, feeding a condition that branches on the agent's stop reason.
fn graph_json() -> Value {
    json!({
        "name": "triage",
        "inputs": [{ "name": "repo", "type": "string", "required": true }],
        "agents": [{
            "id": "triager",
            "name": "Issue triager",
            "description": "Labels and routes inbound issues.",
            "instructions": "You triage issues.",
            "model": "sonnet",
            "provider": "anthropic",
            "working_dir": "/srv/checkout",
            "tools": [
                { "slug": "github.search" },
                { "slug": "github.label", "connection_ref": "conn_bot" }
            ],
            "context": [
                { "kind": "host", "source": "repo_conventions", "params": { "repo": "=inputs.repo" } }
            ],
            "limits": { "max_steps": 8, "max_tool_calls": 20, "tool_timeout_secs": 30 },
            "metadata": { "tier": "fast" }
        }],
        "nodes": [
            { "id": "t", "kind": "trigger", "name": "start", "config": { "trigger_kind": "manual" } },
            {
                "id": "triage", "kind": "agent", "name": "Triage",
                "config": {
                    "agent_ref": "triager",
                    "prompt": "Triage this issue.",
                    "instructions": "Prefer `bug` when a stack trace is present.",
                    "model": "opus",
                    "tools": [{ "slug": "github.search" }],
                    "limits": { "max_steps": 4 },
                    "metadata": { "run_note": "e2e" },
                    "context": [{ "kind": "text", "label": "Repo", "text": "=inputs.repo" }]
                }
            }
        ],
        "edges": [{ "from_node": "t", "to_node": "triage" }]
    })
}

fn parse(graph: Value) -> WorkflowGraph {
    serde_json::from_value(graph).expect("graph deserializes")
}

/// Runs `graph` with `repo` supplied, returning the `triage` node's output item.
async fn run_triage(graph: &WorkflowGraph, caps: &tinyflows::caps::Capabilities) -> Value {
    let compiled = compile(graph).expect("compile");
    let input = RunInput::new(Value::Null).with_inputs(
        json!({ "repo": "acme/api" })
            .as_object()
            .expect("inputs object")
            .clone(),
    );
    let outcome = run(&compiled, input, caps).await.expect("run");
    outcome.output["nodes"]["triage"]["items"][0]["json"].clone()
}

#[tokio::test]
async fn a_graph_declared_agent_reaches_the_harness_merged_and_narrowed() {
    let graph = parse(graph_json());
    let caps = mock_capabilities_with_agent(MockAgentHarness::new());
    let item = run_triage(&graph, &caps).await;
    let request = &item["json"];

    assert_eq!(request["agent"], "triager");
    assert_eq!(
        request["instructions"],
        "You triage issues.\n\nPrefer `bug` when a stack trace is present.",
        "the node's instructions append to the definition's rather than replacing them"
    );
    assert_eq!(request["model"], "opus", "the node overrode the model");
    assert_eq!(
        request["provider"], "anthropic",
        "the definition's provider survived"
    );
    assert_eq!(request["working_dir"], "/srv/checkout");
    assert_eq!(request["prompt"], "Triage this issue.");

    assert_eq!(request["limits"]["max_steps"], 4, "narrowed by the node");
    assert_eq!(
        request["limits"]["max_tool_calls"], 20,
        "left alone by the node"
    );
    assert_eq!(request["limits"]["tool_timeout_secs"], 30);

    assert_eq!(
        request["tools"],
        json!(["github.search"]),
        "the node narrowed two grants to one"
    );

    assert_eq!(request["metadata"]["tier"], "fast");
    assert_eq!(request["metadata"]["run_note"], "e2e");

    let context = request["context"].as_array().expect("context blocks");
    assert_eq!(context.len(), 2, "the definition's block, then the node's");
    assert_eq!(context[0]["kind"], "host");
    assert_eq!(
        context[0]["data"]["repo"], "acme/api",
        "the host source's params resolved their =expression"
    );
    assert_eq!(context[1]["label"], "Repo");
    assert_eq!(context[1]["text"], "acme/api");

    assert_eq!(request["identity"]["node_id"], "triage");
    assert_eq!(request["identity"]["depth"], 0);
}

#[tokio::test]
async fn the_stop_reason_is_addressable_from_a_downstream_node() {
    let graph = parse(graph_json());
    let caps = mock_capabilities_with_agent(MockAgentHarness::new());
    let item = run_triage(&graph, &caps).await;

    // `=item.meta.stop` is what lets a downstream `condition` branch on whether
    // the agent actually reached an answer.
    assert_eq!(item["meta"]["stop"], "finished");
    assert_eq!(item["meta"]["agent_ref"], "triager");

    // The pre-existing envelope keys are untouched.
    assert!(item.get("json").is_some());
    assert!(item.get("text").is_some());
    assert!(item.get("raw").is_some());
}

#[tokio::test]
async fn a_legacy_harness_still_receives_the_raw_config() {
    // The non-breaking guarantee through the whole engine: `MockAgentRunner`
    // implements only `run_agent`, so the default `run` shim forwards the node's
    // resolved config exactly as it did before the typed seam existed.
    let mut json = graph_json();
    json["agents"][0]["context"] = json!([]);
    let graph = parse(json);
    let caps = mock_capabilities_with_agent(MockAgentRunner);
    let item = run_triage(&graph, &caps).await;

    assert_eq!(item["raw"]["agent"], "triager");
    assert_eq!(item["raw"]["connection"], Value::Null);
    let forwarded = &item["raw"]["request"];
    assert_eq!(forwarded["agent_ref"], "triager");
    assert_eq!(forwarded["prompt"], "Triage this issue.");
    assert_eq!(
        forwarded["model"], "opus",
        "the node config is forwarded verbatim, overrides and all"
    );
}

#[tokio::test]
async fn a_graph_without_a_registry_behaves_exactly_as_before() {
    // No `agents` array, no new config keys, no harness — the plain completion
    // path, byte-identical to what it has always produced.
    let graph = parse(json!({
        "nodes": [
            { "id": "t", "kind": "trigger", "name": "start", "config": { "trigger_kind": "manual" } },
            { "id": "a", "kind": "agent", "name": "a", "config": { "prompt": "hi" } }
        ],
        "edges": [{ "from_node": "t", "to_node": "a" }]
    }));
    let compiled = compile(&graph).expect("compile");
    let outcome = run(&compiled, Value::Null, &mock_capabilities())
        .await
        .expect("run");
    let item = &outcome.output["nodes"]["a"]["items"][0]["json"];

    assert_eq!(item["json"]["completion"]["prompt"], "hi");
    assert!(
        item.get("meta").is_none(),
        "the degraded path emits the original three-key envelope, unchanged"
    );
}

#[tokio::test]
async fn the_registry_survives_a_json_round_trip() {
    let graph = parse(graph_json());
    let reserialized: WorkflowGraph =
        serde_json::from_value(serde_json::to_value(&graph).expect("serialize"))
            .expect("deserialize");
    assert_eq!(graph, reserialized);

    let agent = reserialized.agent("triager").expect("agent survives");
    assert_eq!(agent.provider.as_deref(), Some("anthropic"));
    assert_eq!(agent.working_dir.as_deref(), Some("/srv/checkout"));
    assert_eq!(agent.limits.tool_timeout_secs, Some(30));
    assert_eq!(agent.tools.len(), 2);
}

#[tokio::test]
async fn a_host_context_source_a_harness_cannot_expand_fails_loudly() {
    // `MockAgentRunner` implements only `run_agent`, so `resolve_context`
    // defaults to `Ok(None)`. The author declared context that cannot be
    // delivered, and the node must say so rather than run the agent on a
    // silently smaller context — an agent missing its conventions still answers,
    // confidently and wrongly.
    let graph = parse(graph_json());
    let compiled = compile(&graph).expect("compile");
    let input = RunInput::new(Value::Null).with_inputs(
        json!({ "repo": "acme/api" })
            .as_object()
            .expect("inputs object")
            .clone(),
    );
    let err = run(
        &compiled,
        input,
        &mock_capabilities_with_agent(MockAgentRunner),
    )
    .await
    .expect_err("an unresolvable required context source must fail the run");
    let message = err.to_string();
    assert!(message.contains("repo_conventions"), "{message}");
    assert!(message.contains("optional"), "{message}");
}

#[tokio::test]
async fn marking_that_source_optional_makes_it_survivable() {
    let mut json = graph_json();
    json["agents"][0]["context"][0]["optional"] = json!(true);
    let graph = parse(json);
    let item = run_triage(&graph, &mock_capabilities_with_agent(MockAgentRunner)).await;

    // The run completes, and the block is simply absent from the request.
    assert_eq!(item["raw"]["agent"], "triager");
}
