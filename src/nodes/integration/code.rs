//! The `code` node: sandboxed user code.

use async_trait::async_trait;

use crate::error::{EngineError, Result};
use crate::nodes::{NodeContext, NodeExecutor, NodeOutput};

/// Executes sandboxed user code via [`crate::caps::CodeRunner`].
#[derive(Debug, Default, Clone)]
pub struct CodeNode;

#[async_trait]
impl NodeExecutor for CodeNode {
    async fn execute(&self, _ctx: NodeContext<'_>) -> Result<NodeOutput> {
        Err(EngineError::Unimplemented("code node (stage A3)"))
    }
}
