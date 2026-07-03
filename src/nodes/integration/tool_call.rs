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
        // Data-binding: resolve any `=`-expressions in the config against the
        // node's input before reading the tool call's fields.
        let scope = crate::nodes::expr_scope(&ctx);
        let cfg = crate::expr::resolve(&ctx.node.config, &scope);
        let slug = cfg
            .get("slug")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                EngineError::Capability("tool_call node: missing `slug` in config".to_string())
            })?;
        let args = cfg.get("args").cloned().unwrap_or(serde_json::Value::Null);
        let conn = cfg
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

    use super::ToolCallNode;
    use crate::data::Item;
    use crate::error::EngineError;
    use crate::nodes::{NodeContext, NodeExecutor};

    fn tool_node(config: Value) -> Node {
        Node {
            id: "n".into(),
            kind: NodeKind::ToolCall,
            type_version: 1,
            name: "n".into(),
            config,
            ports: vec![],
            position: None,
        }
    }

    #[tokio::test]
    async fn missing_slug_is_a_capability_error() {
        let node = tool_node(json!({ "args": { "x": 1 } }));
        let input = vec![];
        let caps = mock_capabilities();
        let run_meta = Value::Null;
        let ctx = NodeContext {
            node: &node,
            input: &input,
            run: &run_meta,
            caps: &caps,
        };
        let err = ToolCallNode
            .execute(ctx)
            .await
            .expect_err("missing slug must error");
        assert!(
            matches!(err, EngineError::Capability(ref m) if m.contains("slug")),
            "expected a capability error mentioning `slug`, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn resolves_expression_in_args_against_input() {
        // `args.text` is a `=`-expression that must bind to the input item's
        // `name`; the mock tool echoes the args it was invoked with.
        let node = tool_node(json!({ "slug": "x.y", "args": { "text": "=item.name" } }));
        let input = vec![Item::new(json!({ "name": "X" }))];
        let caps = mock_capabilities();
        let run_meta = Value::Null;
        let ctx = NodeContext {
            node: &node,
            input: &input,
            run: &run_meta,
            caps: &caps,
        };
        let out = ToolCallNode.execute(ctx).await.expect("execute");
        assert_eq!(out.items[0].json["tool"], "x.y");
        assert_eq!(out.items[0].json["args"]["text"], "X");
    }

    #[tokio::test]
    async fn missing_args_default_to_null() {
        let node = tool_node(json!({ "slug": "noop" }));
        let input = vec![];
        let caps = mock_capabilities();
        let run_meta = Value::Null;
        let ctx = NodeContext {
            node: &node,
            input: &input,
            run: &run_meta,
            caps: &caps,
        };
        let out = ToolCallNode.execute(ctx).await.expect("execute");
        assert_eq!(out.items.len(), 1);
        assert_eq!(out.items[0].json["tool"], "noop");
        assert_eq!(out.items[0].json["args"], Value::Null);
        assert_eq!(out.items[0].json["connection"], Value::Null);
    }
}
