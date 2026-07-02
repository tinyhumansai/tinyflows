//! The `merge` node: a fan-in barrier.

use async_trait::async_trait;

use crate::error::{EngineError, Result};
use crate::nodes::{NodeContext, NodeExecutor, NodeOutput};

/// Fan-in barrier that combines multiple named inputs.
#[derive(Debug, Default, Clone)]
pub struct MergeNode;

#[async_trait]
impl NodeExecutor for MergeNode {
    async fn execute(&self, _ctx: NodeContext<'_>) -> Result<NodeOutput> {
        Err(EngineError::Unimplemented("merge node (stage A2)"))
    }
}
