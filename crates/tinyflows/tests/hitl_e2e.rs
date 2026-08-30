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

/// A branch that finished alongside an interrupted one must not run again when
/// the run resumes.
///
/// Parallel branches all run before any result is folded, so when one of them
/// pauses the others have genuinely completed. Rescheduling them by position —
/// "everything after the interrupt" — would re-run finished work, and for a node
/// with side effects that means firing them twice. The engine reschedules only
/// the branches that actually interrupted.
///
/// The counting `tool_call` is the instrument: a state diff cannot tell a re-run
/// from a first run when the node is pure, so the assertion is on how many times
/// the capability was invoked.
#[tokio::test]
async fn a_sibling_that_completed_is_not_re_run_on_resume() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Counts invocations so a re-run is visible.
    struct CountingTools(Arc<AtomicUsize>);

    #[async_trait::async_trait]
    impl tinyflows::caps::ToolInvoker for CountingTools {
        async fn invoke(
            &self,
            _slug: &str,
            _args: Value,
            _conn: Option<&str>,
        ) -> tinyflows::error::Result<Value> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(json!({ "ok": true }))
        }
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let mut caps = mock_capabilities();
    caps.tools = Arc::new(CountingTools(calls.clone()));

    // `t` fans out to a gate and to a side-effecting tool call. Both run in the
    // same superstep; the gate pauses, the tool call completes.
    let graph = WorkflowGraph {
        name: "sibling_not_rerun".to_string(),
        nodes: vec![
            node("t", NodeKind::Trigger, Value::Null),
            node(
                "gate",
                NodeKind::OutputParser,
                json!({ "requires_approval": true }),
            ),
            node(
                "effect",
                NodeKind::ToolCall,
                json!({ "slug": "side.effect" }),
            ),
        ],
        edges: vec![edge("t", "gate"), edge("t", "effect")],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");

    let resumable = run_resumable(&compiled, json!({}), &caps)
        .await
        .expect("run_resumable");
    assert_eq!(
        resumable.outcome().pending_approvals,
        vec!["gate".to_string()],
        "the gate should pause the run"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the sibling ran once before the pause"
    );

    resumable
        .resume(vec!["gate".to_string()])
        .await
        .expect("resume");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the sibling had already completed, so resuming must not invoke it a \
         second time"
    );
}

/// A child workflow paused at an approval gate **pauses the parent** instead of
/// failing it, and approving the namespaced gate lets the whole thing finish.
///
/// This used to be a hard error: a node executor had no way to inject an
/// interrupt into the parent run, so a gated child halted the parent rather than
/// suspending it — approval gating was unusable across a sub-workflow boundary.
///
/// The gate surfaces as `<node id>::<child gate id>`. The namespace matters:
/// parent and child are separate graphs with separate id spaces, so an
/// unqualified `approve` from the child would be indistinguishable from a
/// parent gate of the same name.
#[tokio::test]
async fn a_child_paused_at_a_gate_pauses_the_parent_and_resumes() {
    let child = json!({
        "name": "gated_child",
        "nodes": [
            { "id": "ct", "kind": "trigger", "type_version": 1, "name": "ct", "config": null },
            { "id": "cgate", "kind": "output_parser", "type_version": 1, "name": "cgate",
              "config": { "requires_approval": true } },
            { "id": "cdone", "kind": "output_parser", "type_version": 1, "name": "cdone",
              "config": null }
        ],
        "edges": [
            { "from_node": "ct", "from_port": "main", "to_node": "cgate", "to_port": "main" },
            { "from_node": "cgate", "from_port": "main", "to_node": "cdone", "to_port": "main" }
        ]
    });

    let graph = WorkflowGraph {
        name: "parent_of_gated_child".to_string(),
        nodes: vec![
            node("t", NodeKind::Trigger, Value::Null),
            node("sw", NodeKind::SubWorkflow, json!({ "workflow": child })),
            node("after", NodeKind::OutputParser, Value::Null),
        ],
        edges: vec![edge("t", "sw"), edge("sw", "after")],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let caps = mock_capabilities();

    let resumable = run_resumable(&compiled, json!({}), &caps)
        .await
        .expect("a gated child should pause the parent, not fail it");
    assert_eq!(
        resumable.outcome().pending_approvals,
        vec!["sw::cgate".to_string()],
        "the child's gate surfaces namespaced by the sub_workflow node"
    );
    assert!(
        resumable.outcome().output["nodes"]["after"].is_null(),
        "downstream of the sub-workflow must not run while the child is gated"
    );

    let done = resumable
        .resume(vec!["sw::cgate".to_string()])
        .await
        .expect("resume");
    assert!(
        done.pending_approvals.is_empty(),
        "approving the child's gate should settle the run, got {:?}",
        done.pending_approvals
    );
    assert!(
        !done.output["nodes"]["after"]["items"].is_null(),
        "the parent continues past the sub-workflow once the child's gate clears"
    );
}

/// A `per_item` sub-workflow fan-out where several children pause reports
/// **all** their gates at once, not one per resume round-trip.
///
/// Each element gets its own child run, so N elements means N independent gates.
/// Surfacing only the first would make a host discover them one at a time, each
/// costing a full re-run of the whole fan-out.
#[tokio::test]
async fn a_per_item_fan_out_reports_every_paused_child() {
    let child = json!({
        "name": "gated_child",
        "nodes": [
            { "id": "ct", "kind": "trigger", "type_version": 1, "name": "ct", "config": null },
            { "id": "cgate", "kind": "output_parser", "type_version": 1, "name": "cgate",
              "config": { "requires_approval": true } }
        ],
        "edges": [
            { "from_node": "ct", "from_port": "main", "to_node": "cgate", "to_port": "main" }
        ]
    });

    let graph = WorkflowGraph {
        name: "per_item_gated".to_string(),
        nodes: vec![
            node("t", NodeKind::Trigger, Value::Null),
            node("split", NodeKind::SplitOut, json!({ "path": "rows" })),
            node(
                "sw",
                NodeKind::SubWorkflow,
                json!({ "workflow": child, "execution": "per_item", "concurrency": 3 }),
            ),
        ],
        edges: vec![edge("t", "split"), edge("split", "sw")],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let caps = mock_capabilities();

    let resumable = run_resumable(&compiled, json!({ "rows": [1, 2, 3] }), &caps)
        .await
        .expect("the fan-out should pause rather than fail");

    // Every child paused at the same gate id, so the namespaced set collapses to
    // one entry — the point being that it is reported, and reported once, rather
    // than the node failing or hiding the pause behind a single child.
    assert_eq!(
        resumable.outcome().pending_approvals,
        vec!["sw::cgate".to_string()],
        "the fan-out's paused children surface as a namespaced gate"
    );

    let done = resumable
        .resume(vec!["sw::cgate".to_string()])
        .await
        .expect("resume");
    assert!(
        done.pending_approvals.is_empty(),
        "approving the gate clears every child in the fan-out, got {:?}",
        done.pending_approvals
    );
    assert_eq!(
        done.output["nodes"]["sw"]["items"]
            .as_array()
            .map(Vec::len)
            .unwrap_or(0),
        3,
        "all three children ran to completion once approved"
    );
}
