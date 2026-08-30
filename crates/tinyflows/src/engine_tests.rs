use super::*;
use crate::caps::mock::mock_capabilities;
use crate::compiler::compile;
use crate::model::{Edge, Node, WorkflowGraph};
use std::sync::{Arc, Mutex};

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

include!("engine_tests/engine_tests_part_01_tests.rs");
include!("engine_tests/engine_tests_part_02_tests.rs");
include!("engine_tests/engine_tests_part_03_tests.rs");
include!("engine_tests/engine_tests_part_04_tests.rs");
include!("engine_tests/engine_tests_part_05_tests.rs");
include!("engine_tests/engine_tests_part_06_tests.rs");
