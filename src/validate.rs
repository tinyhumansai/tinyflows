//! Structural validation for [`WorkflowGraph`]s, run before compilation.

use std::collections::HashSet;

use crate::error::ValidationError;
use crate::model::{NodeKind, WorkflowGraph};

/// Validates a workflow graph's structure.
///
/// Currently checks: unique node ids, exactly one trigger node, and that every
/// edge references existing nodes. Cycle-legality and per-kind configuration
/// checks are completed in stages A1–A2.
///
/// # Errors
/// Returns the first [`ValidationError`] encountered.
pub fn validate(graph: &WorkflowGraph) -> Result<(), ValidationError> {
    let mut seen = HashSet::new();
    for node in &graph.nodes {
        if !seen.insert(node.id.as_str()) {
            return Err(ValidationError::DuplicateNodeId(node.id.clone()));
        }
    }

    let triggers: Vec<String> = graph
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Trigger)
        .map(|n| n.id.clone())
        .collect();
    match triggers.len() {
        0 => return Err(ValidationError::MissingTrigger),
        1 => {}
        _ => return Err(ValidationError::MultipleTriggers(triggers)),
    }

    for edge in &graph.edges {
        if graph.node(&edge.from_node).is_none() {
            return Err(ValidationError::UnknownNode(edge.from_node.clone()));
        }
        if graph.node(&edge.to_node).is_none() {
            return Err(ValidationError::UnknownNode(edge.to_node.clone()));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
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
}
