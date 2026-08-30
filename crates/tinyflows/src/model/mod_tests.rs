use super::*;

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
fn json_round_trips() {
    let graph = WorkflowGraph {
        schema_version: CURRENT_SCHEMA_VERSION,
        id: Some("wf_1".to_string()),
        name: "demo".to_string(),
        inputs: vec![
            WorkflowInput::new("repo", InputType::String).required(),
            WorkflowInput::new("depth", InputType::Number)
                .with_default(serde_json::json!(3))
                .with_description("How deep to recurse"),
        ],
        agents: Vec::new(),
        nodes: vec![node("t", NodeKind::Trigger), node("a", NodeKind::Agent)],
        edges: vec![Edge {
            from_node: "t".to_string(),
            from_port: "main".to_string(),
            to_node: "a".to_string(),
            to_port: "main".to_string(),
        }],
    };
    let json = serde_json::to_string(&graph).expect("serialize");
    let back: WorkflowGraph = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(graph, back);
}

#[test]
fn edge_ports_default_to_main() {
    let json = r#"{"from_node":"t","to_node":"a"}"#;
    let edge: Edge = serde_json::from_str(json).expect("deserialize");
    assert_eq!(edge.from_port, "main");
    assert_eq!(edge.to_port, "main");
}

#[test]
fn trigger_and_lookup() {
    let graph = WorkflowGraph {
        nodes: vec![node("t", NodeKind::Trigger), node("a", NodeKind::Agent)],
        ..Default::default()
    };
    assert_eq!(graph.trigger().map(|n| n.id.as_str()), Some("t"));
    assert_eq!(graph.node("a").map(|n| n.id.as_str()), Some("a"));
    assert!(graph.node("missing").is_none());
}

#[test]
fn default_stamps_current_schema_version() {
    let graph = WorkflowGraph::default();
    assert_eq!(graph.schema_version, CURRENT_SCHEMA_VERSION);
    assert_eq!(graph.schema_version, 1);
    assert!(graph.id.is_none());
    assert_eq!(graph.name, "");
    assert!(graph.nodes.is_empty());
    assert!(graph.edges.is_empty());
}

#[test]
fn trigger_returns_none_with_zero_triggers() {
    let graph = WorkflowGraph {
        nodes: vec![node("a", NodeKind::Agent)],
        ..Default::default()
    };
    assert!(graph.trigger().is_none());
}

#[test]
fn trigger_returns_none_with_multiple_triggers() {
    let graph = WorkflowGraph {
        nodes: vec![node("t1", NodeKind::Trigger), node("t2", NodeKind::Trigger)],
        ..Default::default()
    };
    assert!(graph.trigger().is_none());
}

#[test]
fn trigger_returns_the_single_trigger() {
    let graph = WorkflowGraph {
        nodes: vec![node("a", NodeKind::Agent), node("t", NodeKind::Trigger)],
        ..Default::default()
    };
    assert_eq!(graph.trigger().map(|n| n.id.as_str()), Some("t"));
}

#[test]
fn successors_lists_direct_edge_targets() {
    let graph = WorkflowGraph {
        nodes: vec![
            node("t", NodeKind::Trigger),
            node("a", NodeKind::Agent),
            node("b", NodeKind::Agent),
        ],
        edges: vec![
            Edge {
                from_node: "t".to_string(),
                from_port: "main".to_string(),
                to_node: "a".to_string(),
                to_port: "main".to_string(),
            },
            Edge {
                from_node: "t".to_string(),
                from_port: "main".to_string(),
                to_node: "b".to_string(),
                to_port: "main".to_string(),
            },
        ],
        ..Default::default()
    };
    assert_eq!(graph.successors("t"), vec!["a", "b"]);
    assert!(graph.successors("a").is_empty());
    assert!(graph.successors("missing").is_empty());
}

#[test]
fn successors_may_repeat_for_parallel_edges() {
    let graph = WorkflowGraph {
        nodes: vec![node("t", NodeKind::Trigger), node("a", NodeKind::Agent)],
        edges: vec![
            Edge {
                from_node: "t".to_string(),
                from_port: "main".to_string(),
                to_node: "a".to_string(),
                to_port: "main".to_string(),
            },
            Edge {
                from_node: "t".to_string(),
                from_port: "other".to_string(),
                to_node: "a".to_string(),
                to_port: "main".to_string(),
            },
        ],
        ..Default::default()
    };
    assert_eq!(graph.successors("t"), vec!["a", "a"]);
}

#[test]
fn graphs_authored_before_inputs_existed_still_load() {
    let graph: WorkflowGraph = serde_json::from_str(
        r#"{"name":"legacy","nodes":[{"id":"t","kind":"trigger","name":"start"}],"edges":[]}"#,
    )
    .expect("deserialize");
    assert!(graph.inputs.is_empty());
}

#[test]
fn declared_inputs_survive_a_json_round_trip() {
    let graph = WorkflowGraph {
        inputs: vec![WorkflowInput::new("repo", InputType::String).required()],
        nodes: vec![node("t", NodeKind::Trigger)],
        ..Default::default()
    };
    let json = serde_json::to_string(&graph).expect("serialize");
    assert!(json.contains(r#""inputs":[{"name":"repo","type":"string""#));
    let back: WorkflowGraph = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, graph);
}

#[test]
fn round_trip_preserves_version_fields() {
    let graph = WorkflowGraph {
        schema_version: CURRENT_SCHEMA_VERSION,
        id: Some("wf_1".to_string()),
        name: "demo".to_string(),
        inputs: Vec::new(),
        agents: Vec::new(),
        nodes: vec![Node {
            id: "t".to_string(),
            kind: NodeKind::Trigger,
            type_version: 3,
            name: "t".to_string(),
            config: serde_json::json!({"mode": "manual"}),
            ports: Vec::new(),
            position: None,
        }],
        edges: Vec::new(),
    };
    let json = serde_json::to_string(&graph).expect("serialize");
    assert!(json.contains("\"schema_version\":1"));
    assert!(json.contains("\"type_version\":3"));
    let back: WorkflowGraph = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(graph, back);
    assert_eq!(back.nodes[0].type_version, 3);
}

#[test]
fn omitted_version_fields_use_serde_defaults() {
    // A graph and node authored before the version fields existed.
    let json = r#"{
            "name": "legacy",
            "nodes": [{"id": "t", "kind": "trigger", "name": "start"}],
            "edges": []
        }"#;
    let graph: WorkflowGraph = serde_json::from_str(json).expect("deserialize");
    assert_eq!(graph.schema_version, CURRENT_SCHEMA_VERSION);
    assert_eq!(graph.nodes[0].type_version, default_type_version());
    assert_eq!(graph.nodes[0].type_version, 1);
    // Other `#[serde(default)]` fields fill in too.
    assert!(graph.id.is_none());
    assert!(graph.nodes[0].config.is_null());
    assert!(graph.nodes[0].ports.is_empty());
    assert!(graph.nodes[0].position.is_none());
}

#[test]
fn edge_from_port_defaults_to_main() {
    let json = r#"{"from_node":"t","to_node":"a","to_port":"custom"}"#;
    let edge: Edge = serde_json::from_str(json).expect("deserialize");
    assert_eq!(edge.from_port, "main");
    assert_eq!(edge.to_port, "custom");
}
