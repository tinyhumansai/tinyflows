//! The `agent` node: an LLM agent turn with optional sub-ports.

use async_trait::async_trait;
use serde_json::Value;

use crate::data::Item;
use crate::error::Result;
use crate::nodes::integration::schema;
use crate::nodes::{NodeContext, NodeExecutor, NodeOutput};

/// Runs an LLM agent turn, optionally composed with **sub-ports** that attach an
/// output parser and tools to the bare completion.
///
/// The node config is the completion request handed to the injected
/// [`LlmProvider`](crate::caps::LlmProvider). On top of that, two sub-ports are
/// wired (config-embedded, so a plain agent node with just a prompt still works
/// unchanged):
///
/// - **tool sub-port** (`config.tools`): the available tools are surfaced to the
///   model in the request. If the model's response elects to call one of the
///   *offered* tools — a `tool_call: { slug, args?, connection_ref? }` object in
///   the response — the agent invokes it once via
///   [`ToolInvoker`](crate::caps::ToolInvoker) and attaches the result under
///   `tool_result`. This is a **single hop** (no unbounded agent loop) — a full
///   multi-turn tool-use loop is a documented follow-up.
/// - **output-parser sub-port** (`config.output_parser`): after the completion
///   (and any tool hop), the resulting value is validated/repaired against
///   `config.output_parser.schema` using the shared [`schema`] routine
///   (validate → one LLM auto-fix → re-validate), honoring
///   `config.output_parser.auto_fix` (default `true`).
///
/// Sub-ports **not** yet wired (documented follow-ups): a `chat_model` sub-port
/// (attached model selection beyond what the request already carries) and a
/// `memory` sub-port (conversation memory injected into the request / persisted
/// across turns). Those require attached-node wiring and/or `StateStore` plumbing
/// and are deliberately left out rather than stubbed.
#[derive(Debug, Default, Clone)]
pub struct AgentNode;

#[async_trait]
impl NodeExecutor for AgentNode {
    async fn execute(&self, ctx: NodeContext<'_>) -> Result<NodeOutput> {
        // Data-binding: resolve any `=`-expressions in the config against the
        // node's input before treating the config as the completion request.
        let scope = crate::nodes::expr_scope(&ctx);
        let cfg = crate::expr::resolve(&ctx.node.config, &scope);
        let conn = cfg.get("connection_ref").and_then(Value::as_str);

        // The node config *is* the completion request; when a `tools` sub-port is
        // configured its descriptors ride along in the request so the model can
        // elect to call one.
        let response = ctx.caps.llm.complete(cfg.clone(), conn).await?;

        // Tool sub-port (single hop): honor a `tool_call` the model returned, but
        // only for a tool that was actually offered in `config.tools`.
        let mut value = response;
        if let Some(tool_call) = value.get("tool_call").cloned() {
            if let Some(slug) = tool_call.get("slug").and_then(Value::as_str) {
                let offered = cfg
                    .get("tools")
                    .and_then(Value::as_array)
                    .is_some_and(|tools| {
                        tools
                            .iter()
                            .any(|t| t.get("slug").and_then(Value::as_str) == Some(slug))
                    });
                if offered {
                    tracing::debug!(slug, "agent tool sub-port: invoking model-elected tool");
                    let args = tool_call.get("args").cloned().unwrap_or(Value::Null);
                    let tool_conn = tool_call
                        .get("connection_ref")
                        .and_then(Value::as_str)
                        .or(conn);
                    let result = ctx.caps.tools.invoke(slug, args, tool_conn).await?;
                    if let Value::Object(map) = &mut value {
                        map.insert("tool_result".to_string(), result);
                    }
                } else {
                    tracing::warn!(
                        slug,
                        "agent tool sub-port: model elected an un-offered tool; ignoring"
                    );
                }
            }
        }

        // Output-parser sub-port: validate/repair the agent output against a schema.
        if let Some(parser) = cfg.get("output_parser").filter(|p| !p.is_null()) {
            if let Some(parser_schema) = parser.get("schema").filter(|s| !s.is_null()) {
                let auto_fix = parser
                    .get("auto_fix")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                let parser_conn = parser
                    .get("connection_ref")
                    .and_then(Value::as_str)
                    .or(conn);
                value = schema::parse_and_validate(
                    value,
                    parser_schema,
                    auto_fix,
                    &ctx.caps.llm,
                    parser_conn,
                )
                .await?;
            }
        }

        Ok(NodeOutput::main(vec![Item::new(value)]))
    }
}

#[cfg(test)]
mod tests {
    use crate::caps::mock::mock_capabilities;
    use crate::compiler::compile;
    use crate::engine::run;
    use crate::model::{Edge, Node, NodeKind, WorkflowGraph};
    use serde_json::{Value, json};

    fn wf(kind: NodeKind, config: Value) -> WorkflowGraph {
        WorkflowGraph {
            nodes: vec![
                Node {
                    id: "t".into(),
                    kind: NodeKind::Trigger,
                    type_version: 1,
                    name: "t".into(),
                    config: Value::Null,
                    ports: vec![],
                    position: None,
                },
                Node {
                    id: "n".into(),
                    kind,
                    type_version: 1,
                    name: "n".into(),
                    config,
                    ports: vec![],
                    position: None,
                },
            ],
            edges: vec![Edge {
                from_node: "t".into(),
                from_port: "main".into(),
                to_node: "n".into(),
                to_port: "main".into(),
            }],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn agent_completes_config_request() {
        let graph = wf(NodeKind::Agent, json!({ "prompt": "hi" }));
        let compiled = compile(&graph).expect("compile");
        let out = run(&compiled, Value::Null, &mock_capabilities())
            .await
            .expect("run");
        assert_eq!(
            out.output["nodes"]["n"]["items"][0]["json"]["completion"]["prompt"],
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
    async fn threads_connection_ref_and_echoes_config() {
        let node = agent_node(json!({ "prompt": "hi", "connection_ref": "acct_9" }));
        let input = vec![Item::new(json!({ "seed": 1 }))];
        let caps = mock_capabilities();
        let run_meta = Value::Null;
        let ctx = NodeContext {
            node: &node,
            input: &input,
            run: &run_meta,
            caps: &caps,
        };
        let out = AgentNode.execute(ctx).await.expect("execute");
        assert_eq!(out.items.len(), 1);
        // The mock LLM echoes the whole config under `completion` and the conn ref.
        assert_eq!(out.items[0].json["completion"]["prompt"], "hi");
        assert_eq!(out.items[0].json["connection"], "acct_9");
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
            caps: &caps,
        };
        let out = AgentNode.execute(ctx).await.expect("execute");
        assert_eq!(out.items[0].json["completion"]["prompt"], "X");
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
            caps: &caps,
        };
        let out = AgentNode.execute(ctx).await.expect("execute");
        assert_eq!(out.items[0].json["connection"], Value::Null);
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
            caps: &caps,
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
            caps,
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
        async fn complete(
            &self,
            _request: Value,
            _conn: Option<&str>,
        ) -> crate::error::Result<Value> {
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
        // Mock ToolInvoker echoes the slug/args it was called with.
        assert_eq!(value["tool_result"]["tool"], "slack.post");
        assert_eq!(value["tool_result"]["args"]["text"], "hi");
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
        assert!(value.get("tool_result").is_none());
    }

    /// An LLM that returns an invalid completion, but a schema-valid value when
    /// asked to coerce (the auto-fix call carries `task == "coerce_to_schema"`).
    struct ParserLlm {
        completion: Value,
        fixed: Value,
    }

    #[async_trait]
    impl LlmProvider for ParserLlm {
        async fn complete(
            &self,
            request: Value,
            _conn: Option<&str>,
        ) -> crate::error::Result<Value> {
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
        assert_eq!(value, json!({ "name": "fixed" }));
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
            caps: &caps,
        };
        let err = AgentNode
            .execute(ctx)
            .await
            .expect_err("unfixable output must error");
        assert!(matches!(err, crate::error::EngineError::Capability(_)));
    }

    #[tokio::test]
    async fn plain_agent_without_sub_ports_is_unchanged() {
        // Back-compat: no tools / output_parser configured ⇒ the completion is
        // emitted verbatim (the mock echoes the request under `completion`).
        let node = agent_node(json!({ "prompt": "hi" }));
        let value = run_agent(&node, &mock_capabilities()).await;
        assert_eq!(value["completion"]["prompt"], "hi");
        assert!(value.get("tool_result").is_none());
    }
}
