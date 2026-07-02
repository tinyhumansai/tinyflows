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
        // A3-basic: the node config is the completion request; sub-port wiring is a later refinement.
        let conn = ctx
            .node
            .config
            .get("connection_ref")
            .and_then(serde_json::Value::as_str);
        let response = ctx.caps.llm.complete(ctx.node.config.clone(), conn).await?;
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
}
