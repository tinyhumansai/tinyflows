//! The `output_parser` node: structures/validates an agent's output.

use async_trait::async_trait;

use crate::error::{EngineError, Result};
use crate::nodes::{NodeContext, NodeExecutor, NodeOutput};

/// Parses / validates an upstream agent's output into a structured shape.
#[derive(Debug, Default, Clone)]
pub struct OutputParserNode;

#[async_trait]
impl NodeExecutor for OutputParserNode {
    async fn execute(&self, _ctx: NodeContext<'_>) -> Result<NodeOutput> {
        Err(EngineError::Unimplemented("output_parser node (stage A3)"))
    }
}
