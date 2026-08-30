use super::approval_request::{decision_from_resume, names};
use super::*;
use serde_json::json;

use crate::caps::ApprovalSubject;
use crate::caps::mock::{MockApprovals, mock_capabilities, mock_capabilities_with_approvals};
use crate::compiler::compile;
use crate::engine::{RunInput, resume, run};
use crate::model::{Edge, Node, NodeKind, WorkflowGraph};

/// A trigger wired into one `approval` node, with `config` on the approval.
///
/// Fills in a stable `request_id` when the caller's config does not already
/// name one, since without a run-scoped identity `build_request` now refuses
/// to guess one (see `a_missing_request_id_and_run_id_is_a_configuration_error`
/// below for the case that tests the refusal itself).
fn wf(mut config: Value) -> WorkflowGraph {
    if let Some(obj) = config.as_object_mut() {
        obj.entry("request_id".to_string())
            .or_insert_with(|| json!("review-request"));
    }
    wf_raw(config)
}

/// [`wf`] without the `request_id` auto-fill, for tests that need to control
/// exactly what identity information the node config and run carry.
fn wf_raw(config: Value) -> WorkflowGraph {
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
                id: "review".into(),
                kind: NodeKind::Approval,
                type_version: 1,
                name: "review".into(),
                config,
                ports: vec![],
                position: None,
            },
        ],
        edges: vec![Edge {
            from_node: "t".into(),
            from_port: "main".into(),
            to_node: "review".into(),
            to_port: "main".into(),
        }],
        ..Default::default()
    }
}

fn request(id: &str) -> ApprovalRequest {
    ApprovalRequest {
        request_id: id.to_string(),
        node_id: "review".to_string(),
        run_id: None,
        title: None,
        prompt: None,
        subject: ApprovalSubject {
            kind: "url".to_string(),
            value: json!("https://example.com/post/1"),
        },
        assignees: vec![],
        metadata: Value::Null,
    }
}

include!("approval_tests/approval_tests_part_01_tests.rs");
include!("approval_tests/approval_tests_part_02_tests.rs");
