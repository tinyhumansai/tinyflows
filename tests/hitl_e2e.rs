#![cfg(feature = "mock")]
//! End-to-end tests for the human-in-the-loop (HITL) approval gating path.
//!
//! A node whose config carries `requires_approval: true` pauses the run until
//! either its id appears in the run input's `approvals` array (the [`run`] path)
//! or a resume delivers approval to the interrupted gate (the [`run_resumable`] /
//! [`ResumableRun::resume`] path). These tests drive both, plus a two-gate
//! sequential flow that is approved one gate at a time.
//!
//! Gated behind the `mock` feature, so plain `cargo test` skips it while
//! `cargo test --all-features` runs it.

use serde_json::{Value, json};
use tinyflows::caps::mock::mock_capabilities;
use tinyflows::compiler::compile;
use tinyflows::engine::{run, run_resumable};
use tinyflows::model::{Edge, Node, NodeKind, TriggerKind, WorkflowGraph};

/// Builds a node with the given id, kind, and config.
fn node(id: &str, kind: NodeKind, config: Value) -> Node {
    Node {
        id: id.to_string(),
        kind,
        type_version: 1,
        name: id.to_string(),
        config,
        ports: vec![],
        position: None,
    }
}

/// Builds a trigger node with the given firing mode.
fn trigger(id: &str, kind: TriggerKind) -> Node {
    node(id, NodeKind::Trigger, json!({ "kind": kind }))
}

/// Builds a passthrough gate node that requires approval before running.
fn gate(id: &str) -> Node {
    node(
        id,
        NodeKind::OutputParser,
        json!({ "requires_approval": true }),
    )
}

/// Builds an edge from `from_node`'s `main` port into `to_node`'s `main` port.
fn edge(from_node: &str, to_node: &str) -> Edge {
    Edge {
        from_node: from_node.to_string(),
        from_port: "main".to_string(),
        to_node: to_node.to_string(),
        to_port: "main".to_string(),
    }
}

/// `run_resumable` on a single-gate flow pauses at the gate; `.resume(vec![gate])`
/// then drives it from the checkpoint so the downstream node runs.
#[tokio::test]
async fn resumable_single_gate_pauses_then_resumes() {
    let graph = WorkflowGraph {
        name: "single_gate".to_string(),
        nodes: vec![
            trigger("start", TriggerKind::Manual),
            gate("approve"),
            node("downstream", NodeKind::OutputParser, Value::Null),
        ],
        edges: vec![edge("start", "approve"), edge("approve", "downstream")],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let caps = mock_capabilities();

    let resumable = run_resumable(&compiled, json!({ "doc": "contract" }), &caps)
        .await
        .expect("run_resumable");

    // The gate is pending and its downstream is blocked.
    assert_eq!(
        resumable.outcome().pending_approvals,
        vec!["approve".to_string()],
        "the single gate should be the only pending approval"
    );
    assert!(
        resumable.outcome().output["nodes"]["downstream"].is_null(),
        "downstream must not run while the gate is pending"
    );

    // Approving the gate drives the run to completion.
    let done = resumable
        .resume(vec!["approve".to_string()])
        .await
        .expect("resume");
    assert!(
        done.pending_approvals.is_empty(),
        "no approvals should remain pending after resuming the gate, got: {:?}",
        done.pending_approvals
    );
    assert!(
        !done.output["nodes"]["downstream"]["items"].is_null(),
        "downstream should run once the gate is approved"
    );
}

/// Two gates in series, approved one at a time via checkpointed resume. Each
/// resume unblocks exactly the current gate and stops at the next one, so the
/// pending set moves forward gate-by-gate and finally empties.
#[tokio::test]
async fn two_gate_sequential_flow_resumes_one_at_a_time() {
    let graph = WorkflowGraph {
        name: "two_gates".to_string(),
        nodes: vec![
            trigger("start", TriggerKind::Manual),
            gate("gate_one"),
            gate("gate_two"),
            node("downstream", NodeKind::OutputParser, Value::Null),
        ],
        edges: vec![
            edge("start", "gate_one"),
            edge("gate_one", "gate_two"),
            edge("gate_two", "downstream"),
        ],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let caps = mock_capabilities();

    let resumable = run_resumable(&compiled, json!({ "n": 1 }), &caps)
        .await
        .expect("run_resumable");

    // Only the first gate is reached and pending; nothing downstream ran.
    assert_eq!(
        resumable.outcome().pending_approvals,
        vec!["gate_one".to_string()],
        "the first gate should pause before the second is reached"
    );
    assert!(
        resumable.outcome().output["nodes"]["gate_two"].is_null(),
        "the second gate should not have been reached yet"
    );
    assert!(
        resumable.outcome().output["nodes"]["downstream"].is_null(),
        "downstream stays blocked behind the gates"
    );

    // Approve the first gate: the run advances and pauses at the second gate.
    let after_first = resumable
        .resume(vec!["gate_one".to_string()])
        .await
        .expect("resume gate_one");
    assert_eq!(
        after_first.pending_approvals,
        vec!["gate_two".to_string()],
        "approving the first gate should advance the pending set to the second"
    );
    assert!(
        after_first.output["nodes"]["downstream"].is_null(),
        "downstream is still blocked behind the second gate"
    );

    // Approve the second gate: the pending set empties and downstream runs.
    let done = resumable
        .resume(vec!["gate_two".to_string()])
        .await
        .expect("resume gate_two");
    assert!(
        done.pending_approvals.is_empty(),
        "the pending set should shrink to empty once both gates are approved, got: {:?}",
        done.pending_approvals
    );
    assert!(
        !done.output["nodes"]["downstream"]["items"].is_null(),
        "downstream should run once both gates are approved"
    );
}

/// The `run` path: an input that already carries `{"approvals":[gate]}` clears the
/// gate up front, so the run completes in one shot and the downstream node runs.
#[tokio::test]
async fn run_with_preapproved_input_completes_immediately() {
    let graph = WorkflowGraph {
        name: "preapproved".to_string(),
        nodes: vec![
            trigger("start", TriggerKind::Manual),
            gate("approve"),
            node("downstream", NodeKind::OutputParser, Value::Null),
        ],
        edges: vec![edge("start", "approve"), edge("approve", "downstream")],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let caps = mock_capabilities();

    let outcome = run(&compiled, json!({ "approvals": ["approve"] }), &caps)
        .await
        .expect("run");

    assert!(
        outcome.pending_approvals.is_empty(),
        "a pre-approved input should leave no pending approvals, got: {:?}",
        outcome.pending_approvals
    );
    assert!(
        !outcome.output["nodes"]["downstream"]["items"].is_null(),
        "downstream should run when the gate is pre-approved in the input"
    );

    // Control: the very same graph with no approvals must pause at the gate.
    let paused = run(&compiled, json!({}), &caps).await.expect("run");
    assert_eq!(
        paused.pending_approvals,
        vec!["approve".to_string()],
        "with no approvals the gate must pause the run"
    );
    assert!(
        paused.output["nodes"]["downstream"].is_null(),
        "downstream must stay blocked when the gate is not approved"
    );
}
