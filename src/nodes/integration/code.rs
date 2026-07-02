//! The `code` node: sandboxed user code.

use async_trait::async_trait;

use crate::caps::CodeLanguage;
use crate::data::Item;
use crate::error::{EngineError, Result};
use crate::nodes::{NodeContext, NodeExecutor, NodeOutput};

/// Executes sandboxed user code via [`crate::caps::CodeRunner`].
#[derive(Debug, Default, Clone)]
pub struct CodeNode;

#[async_trait]
impl NodeExecutor for CodeNode {
    async fn execute(&self, ctx: NodeContext<'_>) -> Result<NodeOutput> {
        let config = &ctx.node.config;
        let language = match config.get("language").and_then(serde_json::Value::as_str) {
            Some("python") => CodeLanguage::Python,
            _ => CodeLanguage::JavaScript,
        };
        let source = config
            .get("source")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let input =
            serde_json::to_value(ctx.input).map_err(|e| EngineError::Capability(e.to_string()))?;
        let result = ctx.caps.code.run(language, source, input).await?;
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
    async fn code_returns_mock_result() {
        let graph = wf(
            NodeKind::Code,
            json!({ "language": "javascript", "source": "return 1;" }),
        );
        let compiled = compile(&graph).expect("compile");
        let out = run(&compiled, json!({ "seed": 1 }), &mock_capabilities())
            .await
            .expect("run");
        assert!(out.output["nodes"]["n"]["items"][0]["json"]["result"].is_array());
    }
}
