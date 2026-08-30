use super::{validate, validate_all};
use crate::error::ValidationError;
use crate::model::{Node, NodeKind, WorkflowGraph};
use serde_json::{Value, json};

/// A trigger plus one configured node of `kind` — the smallest graph that
/// exercises a per-kind config check.
fn graph(kind: NodeKind, config: Value) -> WorkflowGraph {
    let mk = |id: &str, kind: NodeKind, config: Value| Node {
        id: id.to_string(),
        kind,
        type_version: 1,
        name: id.to_string(),
        config,
        ports: Vec::new(),
        position: None,
    };
    WorkflowGraph {
        nodes: vec![
            mk("t", NodeKind::Trigger, Value::Null),
            mk("n", kind, config),
        ],
        ..Default::default()
    }
}

/// The `reason` of the single `InvalidNodeConfig` error, or a panic.
fn reason(kind: NodeKind, config: Value) -> String {
    match validate_all(&graph(kind, config))
        .into_iter()
        .find(|e| matches!(e, ValidationError::InvalidNodeConfig { .. }))
    {
        Some(ValidationError::InvalidNodeConfig { reason, .. }) => reason,
        other => panic!("expected an InvalidNodeConfig error, got {other:?}"),
    }
}

#[test]
fn a_valid_fan_out_passes() {
    assert_eq!(
        validate(&graph(
            NodeKind::Agent,
            json!({ "execution": "per_item", "concurrency": 8, "on_item_error": "collect" })
        )),
        Ok(())
    );
    // `"all"` and `0` are both legal spellings of unbounded.
    for c in [json!("all"), json!(0)] {
        assert_eq!(
            validate(&graph(
                NodeKind::ToolCall,
                json!({ "execution": "per_item", "concurrency": c })
            )),
            Ok(())
        );
    }
}

#[test]
fn per_item_default_kinds_may_carry_fan_out_config_without_declaring_execution() {
    // tool_call / http_request / memory are per-item by default, so the
    // knobs apply without an explicit `execution`.
    for kind in [NodeKind::ToolCall, NodeKind::HttpRequest] {
        assert_eq!(
            validate(&graph(kind.clone(), json!({ "concurrency": 4 }))),
            Ok(()),
            "{kind:?} is per-item by default"
        );
    }
}

#[test]
fn concurrency_on_a_once_node_is_rejected_rather_than_silently_ignored() {
    // `agent` defaults to `once`, so this author asked for parallelism and
    // would otherwise have got none, with no signal at all.
    let reason = reason(NodeKind::Agent, json!({ "concurrency": 8 }));
    assert!(
        reason.contains("no effect") && reason.contains("per_item"),
        "expected a no-effect explanation, got: {reason}"
    );

    // Explicitly opting out is the same story.
    let reason = reason_of(
        NodeKind::ToolCall,
        json!({ "execution": "once", "concurrency": 8 }),
    );
    assert!(reason.contains("no effect"), "got: {reason}");
}

fn reason_of(kind: NodeKind, config: Value) -> String {
    reason(kind, config)
}

#[test]
fn a_malformed_concurrency_is_rejected() {
    for bad in [json!("lots"), json!(-1), json!(1.5), json!(true)] {
        let reason = reason(
            NodeKind::ToolCall,
            json!({ "execution": "per_item", "concurrency": bad }),
        );
        assert!(
            reason.contains("concurrency"),
            "expected a concurrency error for {bad}, got: {reason}"
        );
    }
}

#[test]
fn an_unknown_item_error_policy_is_rejected() {
    let reason = reason(
        NodeKind::ToolCall,
        json!({ "execution": "per_item", "on_item_error": "explode" }),
    );
    assert!(
        reason.contains("on_item_error") && reason.contains("collect"),
        "expected the allowed policies to be listed, got: {reason}"
    );
}

#[test]
fn an_unknown_execution_value_is_rejected() {
    let reason = reason(NodeKind::Agent, json!({ "execution": "parallel" }));
    assert!(
        reason.contains("execution") && reason.contains("per_item"),
        "expected the allowed modes to be listed, got: {reason}"
    );
}

#[test]
fn execution_on_a_kind_that_cannot_map_is_rejected() {
    // A `transform` node does not map over its input; accepting `execution`
    // there would imply a fan-out that never happens.
    let reason = reason(NodeKind::Transform, json!({ "execution": "per_item" }));
    assert!(
        reason.contains("not supported") && reason.contains("transform"),
        "expected the kind to be named in its wire spelling, got: {reason}"
    );
}
