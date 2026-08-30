use super::*;
use crate::model::{Edge, Node};

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

/// A graph with one trigger and no edges — the minimum that passes every
/// structural check, so an inputs test sees only inputs errors.
fn graph_with_inputs(inputs: Vec<crate::model::WorkflowInput>) -> WorkflowGraph {
    WorkflowGraph {
        inputs,
        nodes: vec![node("t", NodeKind::Trigger)],
        ..Default::default()
    }
}

include!("validate_tests/validate_tests_part_01_tests.rs");
include!("validate_tests/validate_tests_part_02_tests.rs");
include!("validate_tests/validate_tests_part_03_tests.rs");
include!("validate_tests/validate_tests_part_04_tests.rs");
