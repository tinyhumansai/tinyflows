//! The `merge` node: an item-concatenating fan-in.

use async_trait::async_trait;

use crate::error::Result;
use crate::nodes::{NodeContext, NodeExecutor, NodeOutput};

/// Fan-in that combines the items arriving from multiple predecessors.
///
/// The engine already concatenates every predecessor's items into `ctx.input`,
/// so at runtime `merge` is a passthrough of that combined stream.
#[derive(Debug, Default, Clone)]
pub struct MergeNode;

#[async_trait]
impl NodeExecutor for MergeNode {
    async fn execute(&self, ctx: NodeContext<'_>) -> Result<NodeOutput> {
        // Fan-in: the engine concatenates all predecessor items into `ctx.input`, so
        // merge emits them combined. (A true multi-branch barrier via waiting edges
        // lands with parallel fan-out support.)
        Ok(NodeOutput::main(ctx.input.to_vec()))
    }
}

#[cfg(test)]
#[path = "merge_tests.rs"]
mod tests;
