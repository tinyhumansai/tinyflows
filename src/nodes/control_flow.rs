//! Native control-flow node executors: if / switch / merge / split_out /
//! transform. These are pure (no host capabilities) and are implemented in
//! stage A2. Each currently returns [`crate::error::EngineError::Unimplemented`].

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

/// Multi-way branch keyed by an expression result.
#[derive(Debug, Default, Clone)]
pub struct SwitchNode;

#[async_trait]
impl NodeExecutor for SwitchNode {
    async fn execute(&self, _ctx: NodeContext<'_>) -> Result<NodeOutput> {
        Err(EngineError::Unimplemented("switch node (stage A2)"))
    }
}

/// Fan-in barrier that combines multiple named inputs.
#[derive(Debug, Default, Clone)]
pub struct MergeNode;

#[async_trait]
impl NodeExecutor for MergeNode {
    async fn execute(&self, _ctx: NodeContext<'_>) -> Result<NodeOutput> {
        Err(EngineError::Unimplemented("merge node (stage A2)"))
    }
}

/// Fan-out that emits one item per element of a list.
#[derive(Debug, Default, Clone)]
pub struct SplitOutNode;

#[async_trait]
impl NodeExecutor for SplitOutNode {
    async fn execute(&self, _ctx: NodeContext<'_>) -> Result<NodeOutput> {
        Err(EngineError::Unimplemented("split_out node (stage A2)"))
    }
}

/// Pure, expression-based data transform over the run state.
#[derive(Debug, Default, Clone)]
pub struct TransformNode;

#[async_trait]
impl NodeExecutor for TransformNode {
    async fn execute(&self, _ctx: NodeContext<'_>) -> Result<NodeOutput> {
        Err(EngineError::Unimplemented("transform node (stage A2)"))
    }
}
