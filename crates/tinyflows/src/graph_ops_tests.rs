use super::*;
use crate::model::NodeKind;
use serde_json::json;

fn node(id: &str, kind: NodeKind) -> Node {
    Node {
        id: id.to_string(),
        kind,
        type_version: 1,
        name: id.to_string(),
        config: Value::Null,
        ports: Vec::new(),
        position: None,
    }
}

fn base() -> WorkflowGraph {
    WorkflowGraph {
        nodes: vec![node("t", NodeKind::Trigger), node("a", NodeKind::Agent)],
        edges: vec![Edge {
            from_node: "t".to_string(),
            from_port: "main".to_string(),
            to_node: "a".to_string(),
            to_port: "main".to_string(),
        }],
        ..Default::default()
    }
}

#[test]
fn set_workflow_inputs_replaces_the_whole_list() {
    use crate::model::{InputType, WorkflowInput};

    let mut base = base();
    base.inputs = vec![WorkflowInput::new("stale", InputType::String)];

    let g = apply_ops(
        &base,
        &[GraphOp::SetWorkflowInputs {
            inputs: vec![
                WorkflowInput::new("repo", InputType::String).required(),
                WorkflowInput::new("depth", InputType::Number).with_default(json!(3)),
            ],
        }],
    )
    .unwrap();

    let names: Vec<&str> = g.inputs.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(names, vec!["repo", "depth"], "replaced, not merged");
    assert_eq!(g.nodes.len(), base.nodes.len(), "nodes are untouched");
}

#[test]
fn set_workflow_inputs_can_clear_declarations() {
    use crate::model::{InputType, WorkflowInput};

    let mut base = base();
    base.inputs = vec![WorkflowInput::new("repo", InputType::String)];

    let g = apply_ops(&base, &[GraphOp::SetWorkflowInputs { inputs: vec![] }]).unwrap();
    assert!(g.inputs.is_empty());
}

#[test]
fn set_workflow_inputs_round_trips_through_its_serde_tag() {
    let op: GraphOp = serde_json::from_str(
        r#"{"op":"set_workflow_inputs","inputs":[{"name":"repo","required":true}]}"#,
    )
    .expect("deserialize");
    assert_eq!(op.name(), "set_workflow_inputs");
    match &op {
        GraphOp::SetWorkflowInputs { inputs } => {
            assert_eq!(inputs.len(), 1);
            assert!(inputs[0].required);
        }
        other => panic!("expected set_workflow_inputs, got {other:?}"),
    }
    let back: GraphOp = serde_json::from_value(serde_json::to_value(&op).unwrap()).unwrap();
    assert_eq!(back, op);
}

#[test]
fn add_node_appends_and_rejects_duplicates() {
    let g = apply_ops(
        &base(),
        &[GraphOp::AddNode {
            node: node("b", NodeKind::Merge),
        }],
    )
    .unwrap();
    assert_eq!(g.nodes.len(), 3);

    let err = apply_ops(
        &base(),
        &[GraphOp::AddNode {
            node: node("a", NodeKind::Agent),
        }],
    )
    .unwrap_err();
    assert_eq!(err.index, 0);
    assert_eq!(err.op, "add_node");
    assert!(matches!(err.kind, GraphOpErrorKind::NodeIdExists(_)));
}

#[test]
fn add_node_rejects_empty_id() {
    let err = apply_ops(
        &base(),
        &[GraphOp::AddNode {
            node: node("", NodeKind::Merge),
        }],
    )
    .unwrap_err();
    assert!(matches!(err.kind, GraphOpErrorKind::EmptyNodeId));
}

#[test]
fn update_node_config_merge_patches() {
    let mut n = node("a", NodeKind::Agent);
    n.config = json!({ "prompt": "hi", "keep": 1 });
    let g0 = WorkflowGraph {
        nodes: vec![node("t", NodeKind::Trigger), n],
        ..Default::default()
    };
    let g = apply_ops(
        &g0,
        &[GraphOp::UpdateNodeConfig {
            id: "a".to_string(),
            config: json!({ "prompt": "bye", "added": true, "keep": null }),
        }],
    )
    .unwrap();
    let cfg = &g.nodes[1].config;
    assert_eq!(cfg["prompt"], "bye");
    assert_eq!(cfg["added"], true);
    assert!(
        cfg.get("keep").is_none(),
        "null leaf deletes the key: {cfg}"
    );
}

#[test]
fn update_node_config_on_null_config_creates_object() {
    let g = apply_ops(
        &base(),
        &[GraphOp::UpdateNodeConfig {
            id: "a".to_string(),
            config: json!({ "x": 1 }),
        }],
    )
    .unwrap();
    assert_eq!(g.nodes[1].config["x"], 1);
}

#[test]
fn update_node_config_missing_node_errors() {
    let err = apply_ops(
        &base(),
        &[GraphOp::UpdateNodeConfig {
            id: "ghost".to_string(),
            config: json!({}),
        }],
    )
    .unwrap_err();
    assert!(matches!(err.kind, GraphOpErrorKind::NodeNotFound(_)));
}

#[test]
fn rename_node_rewires_edges() {
    let g = apply_ops(
        &base(),
        &[GraphOp::RenameNode {
            id: "a".to_string(),
            new_id: "agent1".to_string(),
        }],
    )
    .unwrap();
    assert!(g.nodes.iter().any(|n| n.id == "agent1"));
    assert!(g.nodes.iter().all(|n| n.id != "a"));
    assert_eq!(g.edges[0].to_node, "agent1");
}

#[test]
fn rename_node_rejects_collision_and_missing() {
    let err = apply_ops(
        &base(),
        &[GraphOp::RenameNode {
            id: "a".to_string(),
            new_id: "t".to_string(),
        }],
    )
    .unwrap_err();
    assert!(matches!(err.kind, GraphOpErrorKind::NodeIdExists(_)));

    let err = apply_ops(
        &base(),
        &[GraphOp::RenameNode {
            id: "ghost".to_string(),
            new_id: "z".to_string(),
        }],
    )
    .unwrap_err();
    assert!(matches!(err.kind, GraphOpErrorKind::NodeNotFound(_)));
}

#[test]
fn remove_node_drops_incident_edges() {
    let g = apply_ops(
        &base(),
        &[GraphOp::RemoveNode {
            id: "a".to_string(),
        }],
    )
    .unwrap();
    assert_eq!(g.nodes.len(), 1);
    assert!(g.edges.is_empty(), "incident edge removed");
}

#[test]
fn add_edge_validates_endpoints_and_dupes() {
    // both endpoints must exist
    let err = apply_ops(
        &base(),
        &[GraphOp::AddEdge {
            edge: Edge {
                from_node: "a".to_string(),
                from_port: "main".to_string(),
                to_node: "ghost".to_string(),
                to_port: "main".to_string(),
            },
        }],
    )
    .unwrap_err();
    assert!(matches!(err.kind, GraphOpErrorKind::EdgeEndpointMissing(_)));

    // duplicate rejected
    let err = apply_ops(
        &base(),
        &[GraphOp::AddEdge {
            edge: Edge {
                from_node: "t".to_string(),
                from_port: "main".to_string(),
                to_node: "a".to_string(),
                to_port: "main".to_string(),
            },
        }],
    )
    .unwrap_err();
    assert!(matches!(err.kind, GraphOpErrorKind::EdgeExists(_)));
}

#[test]
fn remove_edge_matches_or_errors() {
    let g = apply_ops(
        &base(),
        &[GraphOp::RemoveEdge {
            from_node: "t".to_string(),
            from_port: "main".to_string(),
            to_node: "a".to_string(),
            to_port: "main".to_string(),
        }],
    )
    .unwrap();
    assert!(g.edges.is_empty());

    let err = apply_ops(
        &base(),
        &[GraphOp::RemoveEdge {
            from_node: "t".to_string(),
            from_port: "main".to_string(),
            to_node: "ghost".to_string(),
            to_port: "main".to_string(),
        }],
    )
    .unwrap_err();
    assert!(matches!(err.kind, GraphOpErrorKind::EdgeNotFound(_)));
}

#[test]
fn set_node_position_sets_coords() {
    let g = apply_ops(
        &base(),
        &[GraphOp::SetNodePosition {
            id: "a".to_string(),
            position: Position { x: 10.0, y: 20.0 },
        }],
    )
    .unwrap();
    assert_eq!(g.nodes[1].position, Some(Position { x: 10.0, y: 20.0 }));
}

#[test]
fn ops_apply_in_sequence_and_base_is_untouched() {
    let b = base();
    let g = apply_ops(
        &b,
        &[
            GraphOp::AddNode {
                node: node("b", NodeKind::Merge),
            },
            GraphOp::AddEdge {
                edge: Edge {
                    from_node: "a".to_string(),
                    from_port: "main".to_string(),
                    to_node: "b".to_string(),
                    to_port: "main".to_string(),
                },
            },
            GraphOp::RenameNode {
                id: "b".to_string(),
                new_id: "merge1".to_string(),
            },
        ],
    )
    .unwrap();
    assert_eq!(g.nodes.len(), 3);
    assert_eq!(g.edges.len(), 2);
    assert!(g.edges.iter().any(|e| e.to_node == "merge1"));
    // base untouched
    assert_eq!(b.nodes.len(), 2);
    assert_eq!(b.edges.len(), 1);
}

#[test]
fn error_index_points_at_the_failing_op() {
    // op 0 ok, op 1 fails.
    let err = apply_ops(
        &base(),
        &[
            GraphOp::SetNodeName {
                id: "a".to_string(),
                name: "Renamed".to_string(),
            },
            GraphOp::RemoveNode {
                id: "ghost".to_string(),
            },
        ],
    )
    .unwrap_err();
    assert_eq!(err.index, 1);
    assert_eq!(err.op, "remove_node");
}

#[test]
fn graph_op_deserializes_from_tagged_json() {
    let op: GraphOp = serde_json::from_value(json!({
        "op": "update_node_config",
        "id": "a",
        "config": { "prompt": "hi" }
    }))
    .unwrap();
    assert!(matches!(op, GraphOp::UpdateNodeConfig { .. }));

    // remove_edge ports default to "main"
    let op: GraphOp = serde_json::from_value(json!({
        "op": "remove_edge", "from_node": "t", "to_node": "a"
    }))
    .unwrap();
    match op {
        GraphOp::RemoveEdge {
            from_port, to_port, ..
        } => {
            assert_eq!(from_port, "main");
            assert_eq!(to_port, "main");
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn id_fields_accept_the_node_id_alias() {
    // remove_node { node_id } is a natural guess and must round-trip to `id`.
    let op: GraphOp = serde_json::from_value(json!({
        "op": "remove_node", "node_id": "trigger"
    }))
    .unwrap();
    assert_eq!(
        op,
        GraphOp::RemoveNode {
            id: "trigger".into()
        }
    );

    // update_node_config { node_id, config }
    let op: GraphOp = serde_json::from_value(json!({
        "op": "update_node_config", "node_id": "a", "config": { "prompt": "hi" }
    }))
    .unwrap();
    assert_eq!(
        op,
        GraphOp::UpdateNodeConfig {
            id: "a".into(),
            config: json!({ "prompt": "hi" }),
        }
    );

    // set_node_name { node_id, name }
    let op: GraphOp = serde_json::from_value(json!({
        "op": "set_node_name", "node_id": "a", "name": "Renamed"
    }))
    .unwrap();
    assert_eq!(
        op,
        GraphOp::SetNodeName {
            id: "a".into(),
            name: "Renamed".into(),
        }
    );

    // set_node_position { node_id, position }
    let op: GraphOp = serde_json::from_value(json!({
        "op": "set_node_position", "node_id": "a", "position": { "x": 1.0, "y": 2.0 }
    }))
    .unwrap();
    assert_eq!(
        op,
        GraphOp::SetNodePosition {
            id: "a".into(),
            position: Position { x: 1.0, y: 2.0 },
        }
    );

    // rename_node accepts node_id + new_node_id aliases together.
    let op: GraphOp = serde_json::from_value(json!({
        "op": "rename_node", "node_id": "a", "new_node_id": "agent1"
    }))
    .unwrap();
    assert_eq!(
        op,
        GraphOp::RenameNode {
            id: "a".into(),
            new_id: "agent1".into(),
        }
    );

    // Canonical `id` / `new_id` still work unchanged.
    let op: GraphOp = serde_json::from_value(json!({
        "op": "rename_node", "id": "a", "new_id": "agent1"
    }))
    .unwrap();
    assert_eq!(
        op,
        GraphOp::RenameNode {
            id: "a".into(),
            new_id: "agent1".into(),
        }
    );
}
