

#[tokio::test]
async fn agent_completes_config_request() {
    let graph = wf(NodeKind::Agent, json!({ "prompt": "hi" }));
    let compiled = compile(&graph).expect("compile");
    let out = run(&compiled, Value::Null, &mock_capabilities())
        .await
        .expect("run");
    assert_eq!(
        out.output["nodes"]["n"]["items"][0]["json"]["json"]["completion"]["prompt"],
        "hi"
    );
}

use super::AgentNode;
use crate::data::Item;
use crate::nodes::{NodeContext, NodeExecutor};

fn agent_node(config: Value) -> Node {
    Node {
        id: "n".into(),
        kind: NodeKind::Agent,
        type_version: 1,
        name: "n".into(),
        config,
        ports: vec![],
        position: None,
    }
}

#[tokio::test]
async fn defaults_to_once_but_per_item_maps_over_input() {
    // Agent defaults to `once` (a single turn regardless of input count)...
    let once = agent_node(json!({ "prompt": "=item.name" }));
    let input = vec![
        Item::new(json!({ "name": "A" })),
        Item::new(json!({ "name": "B" })),
    ];
    let caps = mock_capabilities();
    let run_meta = Value::Null;
    let out = AgentNode
        .execute(NodeContext {
            node: &once,
            input: &input,
            run: &run_meta,
            nodes: &Value::Null,
            caps: &caps,
            agents: &[],
            observer: &crate::observability::NoopObserver,
            token: crate::engine::CancellationToken::new(),
            lane: None,
            resume: None,
            step: 0,
        })
        .await
        .expect("execute");
    assert_eq!(out.items.len(), 1, "once mode emits a single item");
    assert_eq!(out.items[0].json["json"]["completion"]["prompt"], "A");

    // ...but `execution: per_item` runs one turn per input item.
    let per_item = agent_node(json!({ "prompt": "=item.name", "execution": "per_item" }));
    let out = AgentNode
        .execute(NodeContext {
            node: &per_item,
            input: &input,
            run: &run_meta,
            nodes: &Value::Null,
            caps: &caps,
            agents: &[],
            observer: &crate::observability::NoopObserver,
            token: crate::engine::CancellationToken::new(),
            lane: None,
            resume: None,
            step: 0,
        })
        .await
        .expect("execute");
    assert_eq!(out.items.len(), 2, "per_item emits one turn per input");
    assert_eq!(out.items[0].json["json"]["completion"]["prompt"], "A");
    assert_eq!(out.items[1].json["json"]["completion"]["prompt"], "B");
    assert_eq!(out.items[1].paired_item, Some(1));
}

#[tokio::test]
async fn threads_connection_ref_and_echoes_config() {
    let node = agent_node(json!({ "prompt": "hi", "connection_ref": "acct_9" }));
    let input = vec![Item::new(json!({ "seed": 1 }))];
    let caps = mock_capabilities();
    let run_meta = Value::Null;
    let ctx = NodeContext {
        node: &node,
        input: &input,
        run: &run_meta,
        nodes: &Value::Null,
        caps: &caps,
        agents: &[],
        observer: &crate::observability::NoopObserver,
        token: crate::engine::CancellationToken::new(),
        lane: None,
        resume: None,
        step: 0,
    };
    let out = AgentNode.execute(ctx).await.expect("execute");
    assert_eq!(out.items.len(), 1);
    // The mock LLM echoes the whole config under `completion` and the conn
    // ref; under the envelope that structured payload is at `json.*`.
    assert_eq!(out.items[0].json["json"]["completion"]["prompt"], "hi");
    assert_eq!(out.items[0].json["json"]["connection"], "acct_9");
    // The raw completion is preserved verbatim under `raw`.
    assert_eq!(out.items[0].json["raw"]["completion"]["prompt"], "hi");
}

#[tokio::test]
async fn resolves_expression_in_config_against_input() {
    // `prompt` is a `=`-expression bound to the input item's `name`; the mock
    // LLM echoes the resolved request under `completion`.
    let node = agent_node(json!({ "prompt": "=item.name" }));
    let input = vec![Item::new(json!({ "name": "X" }))];
    let caps = mock_capabilities();
    let run_meta = Value::Null;
    let ctx = NodeContext {
        node: &node,
        input: &input,
        run: &run_meta,
        nodes: &Value::Null,
        caps: &caps,
        agents: &[],
        observer: &crate::observability::NoopObserver,
        token: crate::engine::CancellationToken::new(),
        lane: None,
        resume: None,
        step: 0,
    };
    let out = AgentNode.execute(ctx).await.expect("execute");
    assert_eq!(out.items[0].json["json"]["completion"]["prompt"], "X");
}

#[tokio::test]
async fn missing_connection_ref_is_null() {
    let node = agent_node(json!({ "prompt": "hi" }));
    let input = vec![];
    let caps = mock_capabilities();
    let run_meta = Value::Null;
    let ctx = NodeContext {
        node: &node,
        input: &input,
        run: &run_meta,
        nodes: &Value::Null,
        caps: &caps,
        agents: &[],
        observer: &crate::observability::NoopObserver,
        token: crate::engine::CancellationToken::new(),
        lane: None,
        resume: None,
        step: 0,
    };
    let out = AgentNode.execute(ctx).await.expect("execute");
    assert_eq!(out.items[0].json["json"]["connection"], Value::Null);
}

#[tokio::test]
async fn emits_exactly_one_item_regardless_of_input_count() {
    // The agent turn is driven by config, not by mapping over input, so it
    // always emits a single completion item.
    let node = agent_node(json!({ "prompt": "hi" }));
    let input = vec![
        Item::new(json!({ "a": 1 })),
        Item::new(json!({ "b": 2 })),
        Item::new(json!({ "c": 3 })),
    ];
    let caps = mock_capabilities();
    let run_meta = Value::Null;
    let ctx = NodeContext {
        node: &node,
        input: &input,
        run: &run_meta,
        nodes: &Value::Null,
        caps: &caps,
        agents: &[],
        observer: &crate::observability::NoopObserver,
        token: crate::engine::CancellationToken::new(),
        lane: None,
        resume: None,
        step: 0,
    };
    let out = AgentNode.execute(ctx).await.expect("execute");
    assert_eq!(out.items.len(), 1);
    assert_eq!(out.port, None);
}

// --- sub-ports: tool + output_parser ---

use crate::caps::{Capabilities, LlmProvider};
use async_trait::async_trait;
use std::sync::Arc;

fn caps_with_llm(llm: Arc<dyn LlmProvider>) -> Capabilities {
    let mut caps = mock_capabilities();
    caps.llm = llm;
    caps
}

async fn run_agent(node: &Node, caps: &Capabilities) -> Value {
    let input: Vec<Item> = vec![];
    let run_meta = Value::Null;
    let ctx = NodeContext {
        node,
        input: &input,
        run: &run_meta,
        nodes: &Value::Null,
        caps,
        agents: &[],
        observer: &crate::observability::NoopObserver,
        token: crate::engine::CancellationToken::new(),
        lane: None,
        resume: None,
        step: 0,
    };
    AgentNode
        .execute(ctx)
        .await
        .expect("execute")
        .items
        .remove(0)
        .json
}

/// An LLM that returns a fixed `tool_call` directive on the completion call.
struct ToolCallingLlm(Value);

#[async_trait]
impl LlmProvider for ToolCallingLlm {
    async fn complete(&self, _request: Value, _conn: Option<&str>) -> crate::error::Result<Value> {
        Ok(self.0.clone())
    }
}

#[tokio::test]
async fn tool_sub_port_invokes_offered_tool_and_attaches_result() {
    // The model elects to call an offered tool; the agent invokes it once and
    // attaches the (mock) tool output under `tool_result`.
    let node = agent_node(json!({
        "prompt": "do it",
        "tools": [{ "slug": "slack.post" }]
    }));
    let llm = Arc::new(ToolCallingLlm(json!({
        "tool_call": { "slug": "slack.post", "args": { "text": "hi" } }
    })));
    let value = run_agent(&node, &caps_with_llm(llm)).await;
    // Mock ToolInvoker echoes the slug/args it was called with; the tool
    // result lives at the stable `json.tool_result` accessor.
    assert_eq!(value["json"]["tool_result"]["tool"], "slack.post");
    assert_eq!(value["json"]["tool_result"]["args"]["text"], "hi");
}

#[tokio::test]
async fn tool_sub_port_ignores_unoffered_tool() {
    // The model tries to call a tool that was never offered; the agent leaves
    // the output untouched (no `tool_result`).
    let node = agent_node(json!({
        "prompt": "do it",
        "tools": [{ "slug": "slack.post" }]
    }));
    let llm = Arc::new(ToolCallingLlm(json!({
        "tool_call": { "slug": "danger.delete_all" }
    })));
    let value = run_agent(&node, &caps_with_llm(llm)).await;
    assert!(value["json"].get("tool_result").is_none());
}

#[tokio::test]
async fn tool_sub_port_ignores_model_supplied_connection_ref() {
    // Security: a model-supplied `tool_call.connection_ref` must NOT be
    // trusted (prompt-injection could otherwise select an arbitrary host
    // credential). The credential comes from the offered tool descriptor's
    // `connection_ref` when present, else the node's `connection_ref`.
    let node = agent_node(json!({
        "prompt": "do it",
        "connection_ref": "node_acct",
        "tools": [{ "slug": "slack.post", "connection_ref": "trusted_acct" }]
    }));
    let llm = Arc::new(ToolCallingLlm(json!({
        "tool_call": {
            "slug": "slack.post",
            "args": { "text": "hi" },
            "connection_ref": "attacker_acct"
        }
    })));
    let value = run_agent(&node, &caps_with_llm(llm)).await;
    // The mock ToolInvoker echoes the `conn` it was invoked with: it must be
    // the offered descriptor's trusted id, never the model-supplied one.
    assert_eq!(value["json"]["tool_result"]["connection"], "trusted_acct");
}

#[tokio::test]
async fn tool_sub_port_falls_back_to_node_connection_ref() {
    // When the offered tool descriptor carries no `connection_ref`, the node's
    // `connection_ref` is used — still never the model-supplied one.
    let node = agent_node(json!({
        "prompt": "do it",
        "connection_ref": "node_acct",
        "tools": [{ "slug": "slack.post" }]
    }));
    let llm = Arc::new(ToolCallingLlm(json!({
        "tool_call": {
            "slug": "slack.post",
            "args": { "text": "hi" },
            "connection_ref": "attacker_acct"
        }
    })));
    let value = run_agent(&node, &caps_with_llm(llm)).await;
    assert_eq!(value["json"]["tool_result"]["connection"], "node_acct");
}

/// An LLM that returns an invalid completion, but a schema-valid value when
/// asked to coerce (the auto-fix call carries `task == "coerce_to_schema"`).
struct ParserLlm {
    completion: Value,
    fixed: Value,
}

#[async_trait]
impl LlmProvider for ParserLlm {
    async fn complete(&self, request: Value, _conn: Option<&str>) -> crate::error::Result<Value> {
        if request.get("task").and_then(Value::as_str) == Some("coerce_to_schema") {
            Ok(json!({ "value": self.fixed.clone() }))
        } else {
            Ok(self.completion.clone())
        }
    }
}

#[tokio::test]
async fn output_parser_sub_port_repairs_agent_output() {
    // The completion is missing a required `name`; the output-parser sub-port
    // runs a one-shot auto-fix that supplies it.
    let node = agent_node(json!({
        "prompt": "hi",
        "output_parser": { "schema": { "type": "object", "required": ["name"] } }
    }));
    let llm = Arc::new(ParserLlm {
        completion: json!({ "wrong": 1 }),
        fixed: json!({ "name": "fixed" }),
    });
    let value = run_agent(&node, &caps_with_llm(llm)).await;
    // The schema-coerced value is the envelope's structured `json`.
    assert_eq!(value["json"], json!({ "name": "fixed" }));
}

#[tokio::test]
async fn output_parser_sub_port_errors_when_unfixable() {
    let node = agent_node(json!({
        "prompt": "hi",
        "output_parser": { "schema": { "type": "object", "required": ["name"] } }
    }));
    // Completion invalid; "fix" still invalid → the node surfaces an error.
    let llm = Arc::new(ParserLlm {
        completion: json!({ "wrong": 1 }),
        fixed: json!({ "still": "wrong" }),
    });
    let input: Vec<Item> = vec![];
    let run_meta = Value::Null;
    let caps = caps_with_llm(llm);
    let ctx = NodeContext {
        node: &node,
        input: &input,
        run: &run_meta,
        nodes: &Value::Null,
        caps: &caps,
        agents: &[],
        observer: &crate::observability::NoopObserver,
        token: crate::engine::CancellationToken::new(),
        lane: None,
        resume: None,
        step: 0,
    };
    let err = AgentNode
        .execute(ctx)
        .await
        .expect_err("unfixable output must error");
    assert!(matches!(err, crate::error::EngineError::Capability(_)));
}

// --- agent-kind selection (`agent_ref` -> AgentRunner) ---

use crate::caps::mock::{MockAgentRunner, mock_capabilities_with_agent};

#[tokio::test]
async fn agent_ref_routes_to_the_registered_agent_kind() {
    // With an `agent_ref` and an AgentRunner wired, the node runs that named
    // agent (the mock echoes the ref/request) rather than a bare completion.
    let node = agent_node(json!({ "agent_ref": "code_executor", "prompt": "fix the bug" }));
    let caps = mock_capabilities_with_agent(MockAgentRunner);
    let value = run_agent(&node, &caps).await;
    assert_eq!(value["json"]["agent"], "code_executor");
    assert_eq!(value["json"]["request"]["prompt"], "fix the bug");
}

#[tokio::test]
async fn agent_ref_is_ignored_without_a_runner() {
    // No AgentRunner in the bundle → fall back to the LlmProvider completion
    // even though `agent_ref` is present (host without an agent registry).
    let node = agent_node(json!({ "agent_ref": "researcher", "prompt": "hi" }));
    let value = run_agent(&node, &mock_capabilities()).await;
    // MockLlm echo shape, not the MockAgentRunner shape.
    assert_eq!(value["json"]["completion"]["prompt"], "hi");
    assert!(value["json"].get("agent").is_none());
}

#[tokio::test]
async fn empty_agent_ref_falls_back_to_completion() {
    let node = agent_node(json!({ "agent_ref": "", "prompt": "hi" }));
    let caps = mock_capabilities_with_agent(MockAgentRunner);
    let value = run_agent(&node, &caps).await;
    assert_eq!(value["json"]["completion"]["prompt"], "hi");
}

#[tokio::test]
async fn agent_kind_skips_inline_tool_subport() {
    // A registered agent owns its own tool loop, so a `tool_call` directive in
    // its response is NOT re-invoked by the inline sub-port. MockAgentRunner
    // echoes the request; even with `tools` offered, no `tool_result` appears.
    let node = agent_node(json!({
        "agent_ref": "researcher",
        "prompt": "do it",
        "tools": [{ "slug": "web.search" }]
    }));
    let caps = mock_capabilities_with_agent(MockAgentRunner);
    let value = run_agent(&node, &caps).await;
    assert_eq!(value["json"]["agent"], "researcher");
    assert!(value["json"].get("tool_result").is_none());
}

#[tokio::test]
async fn prose_completion_populates_text_accessor() {
    // A model that answers in prose: the envelope exposes it at `text` so a
    // downstream node can bind `=item.text` reliably regardless of provider.
    let node = agent_node(json!({ "prompt": "hi" }));
    let llm = Arc::new(ToolCallingLlm(json!({ "text": "the answer is 42" })));
    let value = run_agent(&node, &caps_with_llm(llm)).await;
    assert_eq!(value["text"], "the answer is 42");
    assert_eq!(value["json"]["text"], "the answer is 42");
    assert_eq!(value["raw"]["text"], "the answer is 42");
}
