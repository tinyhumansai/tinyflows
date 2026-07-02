//! The `condition` node: a two-way IF branch.

use async_trait::async_trait;

use crate::error::{EngineError, Result};
use crate::nodes::{NodeContext, NodeExecutor, NodeOutput};

/// Two-way conditional branch, emitting on the `true` or `false` port.
#[derive(Debug, Default, Clone)]
pub struct ConditionNode;

#[async_trait]
impl NodeExecutor for ConditionNode {
    async fn execute(&self, _ctx: NodeContext<'_>) -> Result<NodeOutput> {
        Err(EngineError::Unimplemented("condition node (stage A2)"))
    }
}
