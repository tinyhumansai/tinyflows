//! The `sub_workflow` node: runs another workflow as a nested sub-graph.

use async_trait::async_trait;

use crate::error::{EngineError, Result};
use crate::nodes::{NodeContext, NodeExecutor, NodeOutput};

/// Runs another workflow as a nested sub-graph.
#[derive(Debug, Default, Clone)]
pub struct SubWorkflowNode;

#[async_trait]
impl NodeExecutor for SubWorkflowNode {
    async fn execute(&self, _ctx: NodeContext<'_>) -> Result<NodeOutput> {
        Err(EngineError::Unimplemented("sub_workflow node (stage A3)"))
    }
}
