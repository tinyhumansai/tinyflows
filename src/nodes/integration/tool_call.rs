//! The `tool_call` node: one specific integration action.

use async_trait::async_trait;

use crate::error::{EngineError, Result};
use crate::nodes::{NodeContext, NodeExecutor, NodeOutput};

/// Invokes one specific integration action via [`crate::caps::ToolInvoker`].
#[derive(Debug, Default, Clone)]
pub struct ToolCallNode;

#[async_trait]
impl NodeExecutor for ToolCallNode {
    async fn execute(&self, _ctx: NodeContext<'_>) -> Result<NodeOutput> {
        Err(EngineError::Unimplemented("tool_call node (stage A3)"))
    }
}
