//! Unit tests for workflow directory layering, document parsing, and the
//! file-backed store's read/write/delete, run-history, and undo behaviour.
//!
//! The mechanics of snapshot files — ordering, the cap, scoping — are tested
//! next to them in `file/revisions_tests.rs`. What is tested here is the part
//! that matters to a caller: that saving captures history at all, and that
//! rolling back lands where the operator expects.
//!
//! Split by theme rather than kept as one file, to stay under this
//! repository's 500-line-per-file ceiling: [`parsing`] (document parsing and the
//! `defaults` block), [`discovery`] (layered directory loading),
//! [`persistence`] (save/delete round-tripping), [`runs`] (run-record
//! listing), [`path_guards`] (escaping-id refusal), and [`history`]
//! (revision snapshotting, undo, rollback). Shared fixtures live here and
//! reach every submodule through `super::*`.

mod discovery;
mod history;
mod parsing;
mod path_guards;
mod persistence;
mod runs;

use std::path::Path;

use serde_json::json;

pub(super) use super::file::{
    HostPolicy, new_run_record, parse_workflow, parse_workflow_with, validate_graph,
};
pub(super) use super::{
    FileWorkflowStore, WorkflowStore, require, require_run, rollback, undo_last,
};
pub(super) use crate::store::types::{
    RunExecutor, RunRecord, RunStatus, WorkflowDefaults, WorkflowError, WorkflowRecord,
};

/// A store rooted in a temporary directory, with definitions and runs kept
/// apart the way the discovered layout keeps them.
pub(super) fn store_in(root: &Path) -> FileWorkflowStore {
    FileWorkflowStore::new(vec![root.join("workflows")], root.join("runs"))
}

/// The smallest document that validates: one trigger, one transform, one edge.
pub(super) fn valid_document(id: &str) -> String {
    json!({
        "id": id,
        "name": "Greet",
        "description": "says hello",
        "nodes": [
            { "id": "t", "kind": "trigger", "name": "start",
              "config": { "trigger_kind": "manual" } },
            { "id": "greet", "kind": "transform", "name": "greet",
              "config": { "set": { "greeting": "=.item.name" } } }
        ],
        "edges": [
            { "from_node": "t", "from_port": "main", "to_node": "greet", "to_port": "main" }
        ]
    })
    .to_string()
}

pub(super) fn write(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}
