//! The `transform` node: a pure, expression-based data transform.

use async_trait::async_trait;

use crate::error::{EngineError, Result};
use crate::nodes::{NodeContext, NodeExecutor, NodeOutput};

/// Pure, expression-based data transform over the run state.
#[derive(Debug, Default, Clone)]
pub struct TransformNode;

#[async_trait]
impl NodeExecutor for TransformNode {
    async fn execute(&self, _ctx: NodeContext<'_>) -> Result<NodeOutput> {
        Err(EngineError::Unimplemented("transform node (stage A2)"))
    }
}
