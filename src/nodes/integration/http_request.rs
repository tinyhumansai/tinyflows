//! The `http_request` node: an outbound HTTP request.

use async_trait::async_trait;

use crate::error::{EngineError, Result};
use crate::nodes::{NodeContext, NodeExecutor, NodeOutput};

/// Performs an outbound HTTP request via [`crate::caps::HttpClient`].
#[derive(Debug, Default, Clone)]
pub struct HttpRequestNode;

#[async_trait]
impl NodeExecutor for HttpRequestNode {
    async fn execute(&self, _ctx: NodeContext<'_>) -> Result<NodeOutput> {
        Err(EngineError::Unimplemented("http_request node (stage A3)"))
    }
}
