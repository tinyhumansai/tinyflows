//! Capability-backed node executors: `agent`, `tool_call`, `http_request`,
//! `code`, `output_parser`, and `sub_workflow`. These reach the outside world
//! through the host capabilities in [`crate::caps`].
//!
//! One module per node kind so parallel work can edit them without conflicts.

pub mod agent;
pub mod code;
pub mod http_request;
pub mod output_parser;
pub(crate) mod schema;
pub mod sub_workflow;
pub mod tool_call;

pub use agent::AgentNode;
pub use code::CodeNode;
pub use http_request::HttpRequestNode;
pub use output_parser::OutputParserNode;
pub use sub_workflow::SubWorkflowNode;
pub use tool_call::ToolCallNode;
