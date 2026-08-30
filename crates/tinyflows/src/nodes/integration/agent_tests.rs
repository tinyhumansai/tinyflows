use crate::caps::mock::mock_capabilities;
use crate::compiler::compile;
use crate::engine::run;
use crate::model::{Edge, Node, NodeKind, WorkflowGraph};
use serde_json::{Value, json};

fn wf(kind: NodeKind, config: Value) -> WorkflowGraph {
    WorkflowGraph {
        nodes: vec![
            Node {
                id: "t".into(),
                kind: NodeKind::Trigger,
                type_version: 1,
                name: "t".into(),
                config: Value::Null,
                ports: vec![],
                position: None,
            },
            Node {
                id: "n".into(),
                kind,
                type_version: 1,
                name: "n".into(),
                config,
                ports: vec![],
                position: None,
            },
        ],
        edges: vec![Edge {
            from_node: "t".into(),
            from_port: "main".into(),
            to_node: "n".into(),
            to_port: "main".into(),
        }],
        ..Default::default()
    }
}

include!("agent_tests/agent_tests_part_01_tests.rs");
include!("agent_tests/agent_tests_part_02_tests.rs");
include!("agent_tests/agent_tests_part_03_tests.rs");
