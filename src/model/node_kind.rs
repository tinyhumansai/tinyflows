//! Node-kind discriminators for the workflow model.

use serde::{Deserialize, Serialize};

/// The category of work a [`crate::model::Node`] performs.
///
/// Kind-specific configuration lives in [`crate::model::Node::config`] as
/// free-form JSON, validated per kind during compilation. Variants serialize as
/// `snake_case` strings on the JSON wire:
///
/// ```
/// use tinyflows::model::NodeKind;
///
/// assert_eq!(serde_json::to_string(&NodeKind::HttpRequest).unwrap(), "\"http_request\"");
/// let kind: NodeKind = serde_json::from_str("\"split_out\"").unwrap();
/// assert_eq!(kind, NodeKind::SplitOut);
/// ```
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trips a value through JSON and asserts the exact wire string.
    fn assert_wire<T>(value: &T, wire: &str)
    where
        T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
    {
        let json = serde_json::to_string(value).expect("serialize");
        assert_eq!(json, format!("\"{wire}\""));
        let back: T = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(&back, value);
    }

    #[test]
    fn node_kind_variants_use_snake_case() {
        assert_wire(&NodeKind::Trigger, "trigger");
        assert_wire(&NodeKind::Agent, "agent");
        assert_wire(&NodeKind::ToolCall, "tool_call");
        assert_wire(&NodeKind::HttpRequest, "http_request");
        assert_wire(&NodeKind::Code, "code");
        assert_wire(&NodeKind::Condition, "condition");
        assert_wire(&NodeKind::Switch, "switch");
        assert_wire(&NodeKind::Merge, "merge");
        assert_wire(&NodeKind::SplitOut, "split_out");
        assert_wire(&NodeKind::Transform, "transform");
        assert_wire(&NodeKind::OutputParser, "output_parser");
        assert_wire(&NodeKind::SubWorkflow, "sub_workflow");
    }

    #[test]
    fn trigger_kind_variants_use_snake_case() {
        assert_wire(&TriggerKind::Manual, "manual");
        assert_wire(&TriggerKind::Schedule, "schedule");
        assert_wire(&TriggerKind::Webhook, "webhook");
        assert_wire(&TriggerKind::AppEvent, "app_event");
        assert_wire(&TriggerKind::Form, "form");
        assert_wire(&TriggerKind::ExecuteByWorkflow, "execute_by_workflow");
        assert_wire(&TriggerKind::ChatMessage, "chat_message");
        assert_wire(&TriggerKind::Evaluation, "evaluation");
        assert_wire(&TriggerKind::System, "system");
    }

    #[test]
    fn unknown_node_kind_discriminator_is_rejected() {
        let err = serde_json::from_str::<NodeKind>("\"not_a_kind\"");
        assert!(err.is_err());
    }

    #[test]
    fn unknown_trigger_kind_discriminator_is_rejected() {
        let err = serde_json::from_str::<TriggerKind>("\"telepathy\"");
        assert!(err.is_err());
    }

    #[test]
    fn camel_case_discriminator_is_rejected() {
        // The wire format is strictly snake_case; the Rust variant name is not it.
        assert!(serde_json::from_str::<NodeKind>("\"HttpRequest\"").is_err());
        assert!(serde_json::from_str::<NodeKind>("\"splitOut\"").is_err());
    }
}
