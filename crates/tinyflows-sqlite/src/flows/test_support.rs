//! Shared test fixtures used across the `flows` seam test files.

use tempfile::TempDir;
use tinyflows::model::{Node, NodeKind, WorkflowGraph};

use std::path::PathBuf;

/// The catalog directory a test opens `flows.db` under.
pub(crate) fn test_dir(tmp: &TempDir) -> PathBuf {
    let dir = tmp.path().join("workspace").join("flows");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

pub(crate) fn trigger_graph() -> WorkflowGraph {
    WorkflowGraph {
        nodes: vec![Node {
            id: "t".to_string(),
            kind: NodeKind::Trigger,
            type_version: 1,
            name: "Trigger".to_string(),
            config: serde_json::Value::Null,
            ports: Vec::new(),
            position: None,
        }],
        ..Default::default()
    }
}

/// An automatic-trigger (`schedule`) graph — `trigger_is_automatic` returns
/// `true` for this, unlike [`trigger_graph`]'s manual (no `trigger_kind`)
/// trigger.
pub(crate) fn automatic_schedule_graph() -> WorkflowGraph {
    WorkflowGraph {
        nodes: vec![Node {
            id: "t".to_string(),
            kind: NodeKind::Trigger,
            type_version: 1,
            name: "Trigger".to_string(),
            config: serde_json::json!({ "trigger_kind": "schedule", "schedule": "0 9 * * *" }),
            ports: Vec::new(),
            position: None,
        }],
        ..Default::default()
    }
}
