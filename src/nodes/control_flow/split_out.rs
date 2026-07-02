//! The `split_out` node: per-item fan-out.

use async_trait::async_trait;

use crate::error::{EngineError, Result};
use crate::nodes::{NodeContext, NodeExecutor, NodeOutput};

/// Fan-out that emits one item per element of a list.
#[derive(Debug, Default, Clone)]
pub struct SplitOutNode;

#[async_trait]
impl NodeExecutor for SplitOutNode {
    async fn execute(&self, _ctx: NodeContext<'_>) -> Result<NodeOutput> {
        Err(EngineError::Unimplemented("split_out node (stage A2)"))
    }
}
