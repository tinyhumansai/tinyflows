//! Capability-backed node executors: agent / tool_call / http_request / code /
//! output_parser / sub_workflow. These call the host through [`crate::caps`] and
//! are implemented in stage A3. Each currently returns
//! [`crate::error::EngineError::Unimplemented`].

use async_trait::async_trait;

use crate::error::{EngineError, Result};
use crate::nodes::{NodeContext, NodeExecutor, NodeOutput};

/// Runs an LLM agent turn with optional chat-model / memory / tool /
/// output-parser sub-ports (via [`crate::caps::LlmProvider`] and
/// [`crate::caps::ToolInvoker`]).
#[derive(Debug, Default, Clone)]
pub struct AgentNode;

#[async_trait]
impl NodeExecutor for AgentNode {
    async fn execute(&self, _ctx: NodeContext<'_>) -> Result<NodeOutput> {
        Err(EngineError::Unimplemented("agent node (stage A3)"))
    }
}

/// Invokes one specific integration action via [`crate::caps::ToolInvoker`].
#[derive(Debug, Default, Clone)]
pub struct ToolCallNode;

#[async_trait]
impl NodeExecutor for ToolCallNode {
    async fn execute(&self, _ctx: NodeContext<'_>) -> Result<NodeOutput> {
        Err(EngineError::Unimplemented("tool_call node (stage A3)"))
    }
}

/// Performs an outbound HTTP request via [`crate::caps::HttpClient`].
#[derive(Debug, Default, Clone)]
pub struct HttpRequestNode;

#[async_trait]
impl NodeExecutor for HttpRequestNode {
    async fn execute(&self, _ctx: NodeContext<'_>) -> Result<NodeOutput> {
        Err(EngineError::Unimplemented("http_request node (stage A3)"))
    }
}

/// Executes sandboxed user code via [`crate::caps::CodeRunner`].
#[derive(Debug, Default, Clone)]
pub struct CodeNode;

#[async_trait]
impl NodeExecutor for CodeNode {
    async fn execute(&self, _ctx: NodeContext<'_>) -> Result<NodeOutput> {
        Err(EngineError::Unimplemented("code node (stage A3)"))
    }
}

/// Parses / validates an upstream agent's output into a structured shape.
#[derive(Debug, Default, Clone)]
pub struct OutputParserNode;

#[async_trait]
impl NodeExecutor for OutputParserNode {
    async fn execute(&self, _ctx: NodeContext<'_>) -> Result<NodeOutput> {
        Err(EngineError::Unimplemented("output_parser node (stage A3)"))
    }
}

/// Runs another workflow as a nested sub-graph.
#[derive(Debug, Default, Clone)]
pub struct SubWorkflowNode;

#[async_trait]
impl NodeExecutor for SubWorkflowNode {
    async fn execute(&self, _ctx: NodeContext<'_>) -> Result<NodeOutput> {
        Err(EngineError::Unimplemented("sub_workflow node (stage A3)"))
    }
}
