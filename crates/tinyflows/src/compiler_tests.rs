use super::*;
use crate::error::{EngineError, ValidationError};
use crate::model::{Node, NodeKind};

#[test]
fn compiles_a_valid_graph() {
    let graph = WorkflowGraph {
        nodes: vec![Node {
            id: "t".to_string(),
            kind: NodeKind::Trigger,
            type_version: 1,
            name: "start".to_string(),
            config: serde_json::Value::Null,
            ports: Vec::new(),
            position: None,
        }],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    assert_eq!(compiled.graph.nodes.len(), 1);
}

#[test]
fn rejects_an_invalid_graph() {
    let graph = WorkflowGraph::default();
    assert!(compile(&graph).is_err());
}

fn node(id: &str, kind: NodeKind) -> Node {
    Node {
        id: id.to_string(),
        kind,
        type_version: 1,
        name: id.to_string(),
        config: serde_json::Value::Null,
        ports: Vec::new(),
        position: None,
    }
}

#[test]
fn compiled_workflow_holds_the_source_graph() {
    let graph = WorkflowGraph {
        id: Some("wf_42".to_string()),
        nodes: vec![node("t", NodeKind::Trigger), node("a", NodeKind::Agent)],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    // The handle carries the validated graph verbatim.
    assert_eq!(compiled.graph, graph);
    assert_eq!(compiled.graph.id.as_deref(), Some("wf_42"));
}

#[test]
fn surfaces_missing_trigger_validation_error() {
    let graph = WorkflowGraph::default();
    assert!(matches!(
        compile(&graph),
        Err(EngineError::Validation(ValidationError::MissingTrigger))
    ));
}

#[test]
fn surfaces_multiple_triggers_validation_error() {
    let graph = WorkflowGraph {
        nodes: vec![node("t1", NodeKind::Trigger), node("t2", NodeKind::Trigger)],
        ..Default::default()
    };
    assert!(matches!(
        compile(&graph),
        Err(EngineError::Validation(ValidationError::MultipleTriggers(
            _
        )))
    ));
}

#[test]
fn surfaces_duplicate_node_id_validation_error() {
    let graph = WorkflowGraph {
        nodes: vec![node("t", NodeKind::Trigger), node("t", NodeKind::Agent)],
        ..Default::default()
    };
    assert!(matches!(
        compile(&graph),
        Err(EngineError::Validation(ValidationError::DuplicateNodeId(_)))
    ));
}
