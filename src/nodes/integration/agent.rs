//! The `agent` node: an LLM agent turn.

use async_trait::async_trait;

use crate::data::Item;
use crate::error::Result;
use crate::nodes::{NodeContext, NodeExecutor, NodeOutput};

/// Runs an LLM agent turn with optional chat-model / memory / tool /
/// output-parser sub-ports (via [`crate::caps::LlmProvider`] and
/// [`crate::caps::ToolInvoker`]).
#[derive(Debug, Default, Clone)]
pub struct AgentNode;

#[async_trait]
impl NodeExecutor for AgentNode {
    async fn execute(&self, ctx: NodeContext<'_>) -> Result<NodeOutput> {
        // Data-binding: resolve any `=`-expressions in the config against the
        // node's input before treating the config as the completion request.
        let scope = crate::nodes::expr_scope(&ctx);
        let cfg = crate::expr::resolve(&ctx.node.config, &scope);
        // A3-basic: the node config is the completion request; sub-port wiring is a later refinement.
        let conn = cfg
            .get("connection_ref")
            .and_then(serde_json::Value::as_str);
        let response = ctx.caps.llm.complete(cfg.clone(), conn).await?;
        Ok(NodeOutput::main(vec![Item::new(response)]))
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
}
