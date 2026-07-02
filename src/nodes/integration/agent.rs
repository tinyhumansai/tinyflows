//! The `agent` node: an LLM agent turn.

use async_trait::async_trait;

use crate::error::{EngineError, Result};
use crate::nodes::{NodeContext, NodeExecutor, NodeOutput};

/// Runs an LLM agent turn with optional chat-model / memory / tool /
/// output-parser sub-ports (via [`crate::caps::LlmProvider`] and
/// [`crate::caps::ToolInvoker`]).
#[derive(Debug, Default, Clone)]
pub struct AgentNode;

#[async_trait]
impl NodeExecutor for AgentNode {
    async fn execute(&self, _ctx: NodeContext<'_>) -> Result<NodeOutput> {
        Err(EngineError::Unimplemented("agent node (stage A3)"))
    }
}
