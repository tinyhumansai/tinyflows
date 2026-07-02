//! Node-kind discriminators for the workflow model.

use serde::{Deserialize, Serialize};

/// The category of work a [`crate::model::Node`] performs.
///
/// Kind-specific configuration lives in [`crate::model::Node::config`] as
/// free-form JSON, validated per kind during compilation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    /// Entry node that starts the workflow. The trigger's firing mode is carried
    /// in config as a [`TriggerKind`].
    Trigger,
    /// Runs an LLM agent turn, with optional chat-model / memory / tool /
    /// output-parser sub-ports.
    Agent,
    /// Invokes one specific integration action (a curated Composio tool).
    ToolCall,
    /// Performs an outbound HTTP request.
    HttpRequest,
    /// Executes sandboxed user code (JavaScript or Python).
    Code,
    /// Two-way conditional branch, emitting on the `true` or `false` port.
    Condition,
    /// Multi-way branch keyed by an expression result.
    Switch,
    /// Fan-in barrier that combines multiple named inputs.
    Merge,
    /// Fan-out that emits one item per element of a list.
    SplitOut,
    /// Pure, expression-based data transform over the run state.
    Transform,
    /// Parses and validates an upstream agent's output into a structured shape.
    OutputParser,
    /// Runs another workflow as a nested sub-graph.
    SubWorkflow,
}

/// How a [`NodeKind::Trigger`] node is fired.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerKind {
    /// Fired manually by the user (e.g. a "Run" button).
    Manual,
    /// Fired on a cron / interval schedule.
    Schedule,
    /// Fired by an inbound HTTP webhook.
    Webhook,
    /// Fired by a connected-app event (e.g. a Composio trigger).
    AppEvent,
    /// Fired on form submission.
    Form,
    /// Fired when invoked by another workflow.
    ExecuteByWorkflow,
    /// Fired by an inbound chat message.
    ChatMessage,
    /// Fired by an evaluation run.
    Evaluation,
    /// Fired by a system event (workflow error, file change, and similar).
    System,
}
