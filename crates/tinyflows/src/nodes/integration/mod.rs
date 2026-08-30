//! Capability-backed node executors: `agent`, `tool_call`, `http_request`,
//! `code`, `shell`, `output_parser`, `sub_workflow`, and `memory`. These reach the
//! outside world through the host capabilities in [`crate::caps`].
//!
//! One module per node kind so parallel work can edit them without conflicts.

pub mod agent;
pub(crate) mod agent_request;
pub mod approval;
pub mod code;
pub(crate) mod envelope;
pub mod gate;
pub mod http_request;
pub mod memory;
pub mod output_parser;
pub(crate) mod schema;
pub mod shell;
pub mod spawn;
pub mod sub_workflow;
pub mod tool_call;

pub use agent::AgentNode;
pub use approval::ApprovalNode;
pub use code::CodeNode;
pub use gate::GateNode;
pub use http_request::HttpRequestNode;
pub use memory::MemoryNode;
pub use output_parser::OutputParserNode;
pub use shell::ShellNode;
pub use spawn::SpawnNode;
pub use sub_workflow::SubWorkflowNode;
pub use tool_call::ToolCallNode;

#[cfg(test)]
#[path = "shell_tests.rs"]
mod shell_tests;
