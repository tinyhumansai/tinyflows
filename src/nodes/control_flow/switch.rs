//! The `switch` node: a multi-way branch.

use async_trait::async_trait;

use crate::error::{EngineError, Result};
use crate::nodes::{NodeContext, NodeExecutor, NodeOutput};

/// Multi-way branch keyed by an expression result.
#[derive(Debug, Default, Clone)]
pub struct SwitchNode;

#[async_trait]
impl NodeExecutor for SwitchNode {
    async fn execute(&self, _ctx: NodeContext<'_>) -> Result<NodeOutput> {
        Err(EngineError::Unimplemented("switch node (stage A2)"))
    }
}
