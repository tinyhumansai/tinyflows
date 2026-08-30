

#[test]
fn accepts_declared_inputs() {
    use crate::model::{InputType, WorkflowInput};

    let graph = graph_with_inputs(vec![
        WorkflowInput::new("repo", InputType::String).required(),
        WorkflowInput::new("depth", InputType::Number).with_default(serde_json::json!(3)),
        WorkflowInput::new("payload", InputType::Json),
    ]);
    assert_eq!(validate(&graph), Ok(()));
}

#[test]
fn rejects_duplicate_input_names() {
    use crate::model::{InputType, WorkflowInput};

    let graph = graph_with_inputs(vec![
        WorkflowInput::new("repo", InputType::String),
        WorkflowInput::new("repo", InputType::Number),
    ]);
    assert_eq!(
        validate(&graph),
        Err(ValidationError::DuplicateInputName("repo".to_string()))
    );
}

#[test]
fn rejects_input_names_expressions_could_not_address() {
    use crate::model::{InputType, WorkflowInput};

    for bad in ["repo-url", "2fa", "", "repo.url"] {
        let graph = graph_with_inputs(vec![WorkflowInput::new(bad, InputType::String)]);
        assert_eq!(
            validate(&graph),
            Err(ValidationError::InvalidInputName(bad.to_string())),
            "{bad} should be rejected"
        );
    }
}

#[test]
fn rejects_default_that_violates_its_own_type() {
    use crate::model::{InputType, WorkflowInput};

    let graph = graph_with_inputs(vec![
        WorkflowInput::new("depth", InputType::Number).with_default(serde_json::json!("3")),
    ]);
    assert_eq!(
        validate(&graph),
        Err(ValidationError::InputDefaultTypeMismatch {
            name: "depth".to_string(),
            expected: "number",
        })
    );
}

#[test]
fn rejects_required_input_with_a_default() {
    use crate::model::{InputType, WorkflowInput};

    let graph = graph_with_inputs(vec![
        WorkflowInput::new("repo", InputType::String)
            .required()
            .with_default(serde_json::json!("acme/api")),
    ]);
    assert_eq!(
        validate(&graph),
        Err(ValidationError::RequiredInputWithDefault(
            "repo".to_string()
        ))
    );
}

#[test]
fn collects_every_input_error_in_one_pass() {
    use crate::model::{InputType, WorkflowInput};

    let graph = graph_with_inputs(vec![
        WorkflowInput::new("repo-url", InputType::String),
        WorkflowInput::new("depth", InputType::Number).with_default(serde_json::json!("3")),
        WorkflowInput::new("depth", InputType::Number),
    ]);
    let errors = validate_all(&graph);
    assert_eq!(errors.len(), 3, "got {errors:?}");
    assert!(errors.contains(&ValidationError::InvalidInputName("repo-url".to_string())));
    assert!(errors.contains(&ValidationError::InputDefaultTypeMismatch {
        name: "depth".to_string(),
        expected: "number",
    }));
    assert!(errors.contains(&ValidationError::DuplicateInputName("depth".to_string())));
}

#[test]
fn accepts_a_minimal_valid_graph() {
    let graph = WorkflowGraph {
        nodes: vec![node("t", NodeKind::Trigger), node("a", NodeKind::Agent)],
        edges: vec![Edge {
            from_node: "t".to_string(),
            from_port: "main".to_string(),
            to_node: "a".to_string(),
            to_port: "main".to_string(),
        }],
        ..Default::default()
    };
    assert_eq!(validate(&graph), Ok(()));
}

#[test]
fn rejects_missing_trigger() {
    let graph = WorkflowGraph {
        nodes: vec![node("a", NodeKind::Agent)],
        ..Default::default()
    };
    assert_eq!(validate(&graph), Err(ValidationError::MissingTrigger));
}

#[test]
fn rejects_multiple_triggers() {
    let graph = WorkflowGraph {
        nodes: vec![node("t1", NodeKind::Trigger), node("t2", NodeKind::Trigger)],
        ..Default::default()
    };
    assert!(matches!(
        validate(&graph),
        Err(ValidationError::MultipleTriggers(_))
    ));
}

#[test]
fn rejects_duplicate_ids() {
    let graph = WorkflowGraph {
        nodes: vec![node("t", NodeKind::Trigger), node("t", NodeKind::Agent)],
        ..Default::default()
    };
    assert_eq!(
        validate(&graph),
        Err(ValidationError::DuplicateNodeId("t".to_string()))
    );
}

#[test]
fn rejects_dangling_edge() {
    let graph = WorkflowGraph {
        nodes: vec![node("t", NodeKind::Trigger)],
        edges: vec![Edge {
            from_node: "t".to_string(),
            from_port: "main".to_string(),
            to_node: "ghost".to_string(),
            to_port: "main".to_string(),
        }],
        ..Default::default()
    };
    assert_eq!(
        validate(&graph),
        Err(ValidationError::UnknownNode("ghost".to_string()))
    );
}

#[test]
fn rejects_empty_graph_as_missing_trigger() {
    let graph = WorkflowGraph::default();
    assert_eq!(validate(&graph), Err(ValidationError::MissingTrigger));
}

#[test]
fn rejects_edge_with_unknown_from_node() {
    let graph = WorkflowGraph {
        nodes: vec![node("t", NodeKind::Trigger)],
        edges: vec![Edge {
            from_node: "ghost".to_string(),
            from_port: "main".to_string(),
            to_node: "t".to_string(),
            to_port: "main".to_string(),
        }],
        ..Default::default()
    };
    assert_eq!(
        validate(&graph),
        Err(ValidationError::UnknownNode("ghost".to_string()))
    );
}

#[test]
fn rejects_edge_with_unknown_to_node() {
    let graph = WorkflowGraph {
        nodes: vec![node("t", NodeKind::Trigger), node("a", NodeKind::Agent)],
        edges: vec![Edge {
            from_node: "a".to_string(),
            from_port: "main".to_string(),
            to_node: "ghost".to_string(),
            to_port: "main".to_string(),
        }],
        ..Default::default()
    };
    assert_eq!(
        validate(&graph),
        Err(ValidationError::UnknownNode("ghost".to_string()))
    );
}

#[test]
fn multiple_triggers_error_carries_all_ids() {
    let graph = WorkflowGraph {
        nodes: vec![
            node("t1", NodeKind::Trigger),
            node("t2", NodeKind::Trigger),
            node("t3", NodeKind::Trigger),
        ],
        ..Default::default()
    };
    match validate(&graph) {
        Err(ValidationError::MultipleTriggers(ids)) => {
            assert_eq!(ids, vec!["t1", "t2", "t3"]);
        }
        other => panic!("expected MultipleTriggers, got {other:?}"),
    }
}

fn sub_workflow_node(config: serde_json::Value) -> Node {
    let mut n = node("sw", NodeKind::SubWorkflow);
    n.config = config;
    n
}

fn graph_with_sub_workflow(config: serde_json::Value) -> WorkflowGraph {
    WorkflowGraph {
        nodes: vec![node("t", NodeKind::Trigger), sub_workflow_node(config)],
        ..Default::default()
    }
}

#[test]
fn sub_workflow_accepts_inline_workflow() {
    let graph = graph_with_sub_workflow(serde_json::json!({
        "workflow": { "nodes": [], "edges": [] }
    }));
    assert_eq!(validate(&graph), Ok(()));
}

#[test]
fn sub_workflow_accepts_workflow_id() {
    let graph = graph_with_sub_workflow(serde_json::json!({ "workflow_id": "child-1" }));
    assert_eq!(validate(&graph), Ok(()));
}

#[test]
fn sub_workflow_rejects_both_inline_and_id() {
    let graph = graph_with_sub_workflow(serde_json::json!({
        "workflow": { "nodes": [], "edges": [] },
        "workflow_id": "child-1"
    }));
    assert!(matches!(
        validate(&graph),
        Err(ValidationError::InvalidNodeConfig { .. })
    ));
}

#[test]
fn sub_workflow_rejects_neither_inline_nor_id() {
    // A blank `workflow_id` counts as absent.
    let graph = graph_with_sub_workflow(serde_json::json!({ "workflow_id": "" }));
    assert!(matches!(
        validate(&graph),
        Err(ValidationError::InvalidNodeConfig { .. })
    ));
    let graph = graph_with_sub_workflow(serde_json::Value::Null);
    assert!(matches!(
        validate(&graph),
        Err(ValidationError::InvalidNodeConfig { .. })
    ));
}

fn memory_node(id: &str, config: serde_json::Value) -> Node {
    let mut n = node(id, NodeKind::Memory);
    n.config = config;
    n
}

fn graph_with_memory_node(config: serde_json::Value) -> WorkflowGraph {
    WorkflowGraph {
        nodes: vec![node("t", NodeKind::Trigger), memory_node("mem", config)],
        edges: vec![Edge {
            from_node: "t".to_string(),
            from_port: "main".to_string(),
            to_node: "mem".to_string(),
            to_port: "main".to_string(),
        }],
        ..Default::default()
    }
}

// --- the hard invariant: remember/forget may never target scope "user" ---

#[test]
fn memory_rejects_remember_user_scope() {
    let graph = graph_with_memory_node(serde_json::json!({
        "operation": "remember", "scope": "user", "key": "k", "value": 1
    }));
    let err = validate(&graph).expect_err("remember·user must be rejected");
    match err {
        ValidationError::InvalidNodeConfig { node, reason } => {
            assert_eq!(node, "mem");
            assert!(reason.contains("\"user\""), "reason: {reason}");
            assert!(reason.contains("remember"), "reason: {reason}");
        }
        other => panic!("expected InvalidNodeConfig, got {other:?}"),
    }
}

#[test]
fn memory_rejects_forget_user_scope() {
    let graph = graph_with_memory_node(serde_json::json!({
        "operation": "forget", "scope": "user", "key": "k"
    }));
    let err = validate(&graph).expect_err("forget·user must be rejected");
    match err {
        ValidationError::InvalidNodeConfig { node, reason } => {
            assert_eq!(node, "mem");
            assert!(reason.contains("\"user\""), "reason: {reason}");
            assert!(reason.contains("forget"), "reason: {reason}");
        }
        other => panic!("expected InvalidNodeConfig, got {other:?}"),
    }
}

#[test]
fn memory_rejects_remember_flows_scope() {
    // "flows" is a read-only cross-flow scope — a write to it must be
    // rejected at validate time, not just backstopped by the host adapter.
    let graph = graph_with_memory_node(serde_json::json!({
        "operation": "remember", "scope": "flows", "key": "k", "value": 1
    }));
    let err = validate(&graph).expect_err("remember·flows must be rejected");
    match err {
        ValidationError::InvalidNodeConfig { node, reason } => {
            assert_eq!(node, "mem");
            assert!(reason.contains("\"flows\""), "reason: {reason}");
            assert!(reason.contains("remember"), "reason: {reason}");
        }
        other => panic!("expected InvalidNodeConfig, got {other:?}"),
    }
}

#[test]
fn memory_rejects_forget_flows_scope() {
    let graph = graph_with_memory_node(serde_json::json!({
        "operation": "forget", "scope": "flows", "key": "k"
    }));
    let err = validate(&graph).expect_err("forget·flows must be rejected");
    match err {
        ValidationError::InvalidNodeConfig { node, reason } => {
            assert_eq!(node, "mem");
            assert!(reason.contains("\"flows\""), "reason: {reason}");
            assert!(reason.contains("forget"), "reason: {reason}");
        }
        other => panic!("expected InvalidNodeConfig, got {other:?}"),
    }
}

#[test]
fn memory_accepts_remember_flow_scope() {
    let graph = graph_with_memory_node(serde_json::json!({
        "operation": "remember", "scope": "flow", "key": "k", "value": { "v": 1 }
    }));
    assert_eq!(validate(&graph), Ok(()));
}

#[test]
fn memory_accepts_forget_flow_scope() {
    let graph = graph_with_memory_node(serde_json::json!({
        "operation": "forget", "scope": "flow", "key": "k"
    }));
    assert_eq!(validate(&graph), Ok(()));
}

#[test]
fn memory_rejects_unknown_scope_value() {
    let graph = graph_with_memory_node(serde_json::json!({
        "operation": "recall", "scope": "everyone", "query": "x"
    }));
    assert!(matches!(
        validate(&graph),
        Err(ValidationError::InvalidNodeConfig { .. })
    ));
}

// --- required-field checks per operation ---

#[test]
fn memory_recall_accepts_user_and_flows_scope() {
    // Only remember/forget are scope-restricted; reads may target any
    // declared scope, including the read-only ones.
    for scope in ["user", "flow", "flows"] {
        let graph = graph_with_memory_node(serde_json::json!({
            "operation": "recall", "scope": scope, "query": "x"
        }));
        assert_eq!(
            validate(&graph),
            Ok(()),
            "scope {scope} should be valid for recall"
        );
    }
}

#[test]
fn memory_requires_operation() {
    let graph = graph_with_memory_node(serde_json::json!({ "scope": "flow" }));
    match validate(&graph) {
        Err(ValidationError::InvalidNodeConfig { node, reason }) => {
            assert_eq!(node, "mem");
            assert!(reason.contains("operation"), "reason: {reason}");
        }
        other => panic!("expected InvalidNodeConfig, got {other:?}"),
    }
}

#[test]
fn memory_rejects_unknown_operation() {
    let graph = graph_with_memory_node(serde_json::json!({ "operation": "levitate" }));
    assert!(matches!(
        validate(&graph),
        Err(ValidationError::InvalidNodeConfig { .. })
    ));
}

#[test]
fn memory_recall_requires_scope() {
    let graph = graph_with_memory_node(serde_json::json!({
        "operation": "recall", "query": "x"
    }));
    match validate(&graph) {
        Err(ValidationError::InvalidNodeConfig { node, reason }) => {
            assert_eq!(node, "mem");
            assert!(reason.contains("scope"), "reason: {reason}");
        }
        other => panic!("expected InvalidNodeConfig, got {other:?}"),
    }
}
