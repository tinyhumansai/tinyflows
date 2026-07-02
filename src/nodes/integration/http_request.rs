//! The `http_request` node: an outbound HTTP request.

use async_trait::async_trait;

use crate::data::Item;
use crate::error::Result;
use crate::nodes::{NodeContext, NodeExecutor, NodeOutput};

/// Performs an outbound HTTP request via [`crate::caps::HttpClient`].
#[derive(Debug, Default, Clone)]
pub struct HttpRequestNode;

#[async_trait]
impl NodeExecutor for HttpRequestNode {
    async fn execute(&self, ctx: NodeContext<'_>) -> Result<NodeOutput> {
        // The node's config is the request descriptor; the host's HttpClient interprets it.
        let conn = ctx
            .node
            .config
            .get("connection_ref")
            .and_then(serde_json::Value::as_str);
        let response = ctx.caps.http.request(ctx.node.config.clone(), conn).await?;
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
    async fn http_request_returns_mock_response() {
        let graph = wf(
            NodeKind::HttpRequest,
            json!({ "method": "GET", "url": "https://example.com" }),
        );
        let compiled = compile(&graph).expect("compile");
        let out = run(&compiled, json!({ "seed": 1 }), &mock_capabilities())
            .await
            .expect("run");
        assert_eq!(out.output["nodes"]["n"]["items"][0]["json"]["status"], 200);
    }
}
