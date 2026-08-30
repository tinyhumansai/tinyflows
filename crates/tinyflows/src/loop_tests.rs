use super::*;
use crate::model::{Edge, Node};

/// Builds a node with no config — the shape most nodes in these graphs take.
fn node(id: &str, kind: NodeKind) -> Node {
    node_cfg(id, kind, serde_json::Value::Null)
}

/// Builds a node with an explicit config, which a `loop` node needs.
fn node_cfg(id: &str, kind: NodeKind, config: serde_json::Value) -> Node {
    Node {
        id: id.to_string(),
        kind,
        type_version: 1,
        name: id.to_string(),
        config,
        ports: Vec::new(),
        position: None,
    }
}

fn edge_on(from: &str, port: &str, to: &str) -> Edge {
    Edge {
        from_node: from.to_string(),
        from_port: port.to_string(),
        to_node: to.to_string(),
        to_port: "main".to_string(),
    }
}

include!("loop_tests/loop_tests_part_01_tests.rs");
include!("loop_tests/loop_tests_part_02_tests.rs");
