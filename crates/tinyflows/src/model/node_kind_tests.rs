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
    assert_wire(&NodeKind::Shell, "shell");
    assert_wire(&NodeKind::Condition, "condition");
    assert_wire(&NodeKind::Switch, "switch");
    assert_wire(&NodeKind::Merge, "merge");
    assert_wire(&NodeKind::SplitOut, "split_out");
    assert_wire(&NodeKind::Loop, "loop");
    assert_wire(&NodeKind::Transform, "transform");
    assert_wire(&NodeKind::OutputParser, "output_parser");
    assert_wire(&NodeKind::SubWorkflow, "sub_workflow");
    assert_wire(&NodeKind::Memory, "memory");
    assert_wire(&NodeKind::Dedup, "dedup");
    assert_wire(&NodeKind::Void, "void");
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
