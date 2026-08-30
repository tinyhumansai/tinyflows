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
#[path = "code_tests.rs"]
mod tests;
