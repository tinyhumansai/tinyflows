//! The `tool_call` node: one specific integration action.

use async_trait::async_trait;

use crate::data::Item;
use crate::error::{EngineError, Result};
use crate::nodes::{NodeContext, NodeExecutor, NodeOutput};

/// Invokes one specific integration action via [`crate::caps::ToolInvoker`].
#[derive(Debug, Default, Clone)]
pub struct ToolCallNode;

#[async_trait]
impl NodeExecutor for ToolCallNode {
    async fn execute(&self, ctx: NodeContext<'_>) -> Result<NodeOutput> {
        let slug = ctx
            .node
            .config
            .get("slug")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                EngineError::Capability("tool_call node: missing `slug` in config".to_string())
            })?;
        let args = ctx
            .node
            .config
            .get("args")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let conn = ctx
            .node
            .config
            .get("connection_ref")
            .and_then(serde_json::Value::as_str);
        let result = ctx.caps.tools.invoke(slug, args, conn).await?;
        Ok(NodeOutput::main(vec![Item::new(result)]))
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
                    name: "t".into(),
                    config: Value::Null,
                    ports: vec![],
                    position: None,
                },
                Node {
                    id: "n".into(),
                    kind,
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
    async fn tool_call_invokes_slug_with_args() {
        let graph = wf(
            NodeKind::ToolCall,
            json!({ "slug": "slack.post", "args": { "x": 1 } }),
        );
        let compiled = compile(&graph).expect("compile");
        let out = run(&compiled, Value::Null, &mock_capabilities())
            .await
            .expect("run");
        assert_eq!(
            out.output["nodes"]["n"]["items"][0]["json"]["tool"],
            "slack.post"
        );
        assert_eq!(out.output["nodes"]["n"]["items"][0]["json"]["args"]["x"], 1);
    }

    #[tokio::test]
    async fn tool_call_threads_connection_ref() {
        let graph = wf(
            NodeKind::ToolCall,
            json!({ "slug": "slack.post", "connection_ref": "composio:slack:acct_1" }),
        );
        let compiled = compile(&graph).expect("compile");
        let out = run(&compiled, Value::Null, &mock_capabilities())
            .await
            .expect("run");
        assert_eq!(
            out.output["nodes"]["n"]["items"][0]["json"]["connection"],
            "composio:slack:acct_1"
        );
    }
}
