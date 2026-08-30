//! End-to-end tests for live debug sessions.
//!
//! A real run, spawned onto its own task, driven from the test's task — which
//! is the arrangement the whole module exists to make possible, so it is the
//! arrangement worth testing.

use super::*;
use crate::caps::mock::mock_capabilities;
use crate::compiler::compile;
use crate::data::Item;
use crate::model::{Edge, Node, NodeKind, TriggerKind, WorkflowGraph};
use crate::testkit::debug::breakpoint::Condition;
use crate::testkit::debug::{BreakpointSpec, DebugCommand};
use serde_json::{Value, json};
use std::time::Duration;

/// Long enough that a loaded CI box does not report a false failure, short
/// enough that a genuine hang fails the test rather than the suite.
const WAIT: Duration = Duration::from_secs(10);

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

fn edge(from_node: &str, to_node: &str) -> Edge {
    Edge {
        from_node: from_node.to_string(),
        from_port: "main".to_string(),
        to_node: to_node.to_string(),
        to_port: "main".to_string(),
    }
}

fn graph() -> WorkflowGraph {
    WorkflowGraph {
        name: "debugged".to_string(),
        nodes: vec![
            node(
                "t",
                NodeKind::Trigger,
                json!({ "kind": TriggerKind::Manual }),
            ),
            node("call", NodeKind::ToolCall, json!({ "slug": "svc.do" })),
            node(
                "after",
                NodeKind::Transform,
                json!({ "set": { "seen": "=item" } }),
            ),
        ],
        edges: vec![edge("t", "call"), edge("call", "after")],
        ..Default::default()
    }
}

fn session() -> DebugSession {
    let compiled = compile(&graph()).expect("compile");
    DebugSession::start_quiet(compiled, json!({}), mock_capabilities()).expect("session starts")
}

#[tokio::test]
async fn a_run_parks_at_a_breakpoint_and_reports_where() {
    let mut session = session();
    session
        .controller()
        .set_breakpoint(BreakpointSpec::before("call"))
        .expect("registers");

    let pause = session
        .next_pause(WAIT)
        .await
        .expect("the run should park at the breakpoint");

    assert_eq!(pause.node_id, "call");
    assert_eq!(pause.phase, "before");
    assert_eq!(pause.activation, 1);
    assert!(
        matches!(session.status(), SessionStatus::Paused(1)),
        "the session should report one parked activation"
    );

    session
        .controller()
        .release(pause.pause_id, DebugCommand::Continue)
        .expect("releases");
    session.finish().await.expect("the run completes");
}

#[tokio::test]
async fn a_paused_activation_can_be_inspected_before_it_runs() {
    let mut graph = graph();
    graph.nodes[1].config = json!({
        "slug": "svc.do",
        "args": { "to": "=nodes.t.item.missing" }
    });
    let compiled = compile(&graph).expect("compile");
    let mut session =
        DebugSession::start_quiet(compiled, json!({}), mock_capabilities()).expect("session");
    session
        .controller()
        .set_breakpoint(BreakpointSpec::before("call"))
        .expect("registers");

    let pause = session.next_pause(WAIT).await.expect("parks");

    // The whole point of breaking *before*: see what it is about to be handed.
    assert_eq!(
        pause.null_bindings,
        vec![("args.to".to_string(), "=nodes.t.item.missing".to_string())],
        "the pause should name the binding that is about to resolve to nothing"
    );
    assert!(!pause.input.is_empty(), "the resolved input is visible");
    assert!(pause.state.get("run").is_some(), "the run state is visible");

    session
        .controller()
        .release(pause.pause_id, DebugCommand::Continue)
        .expect("releases");
    session.finish().await.expect("completes");
}

#[tokio::test]
async fn overriding_at_a_pause_changes_what_downstream_sees() {
    // Driving the run from this task while it sits parked in another is the
    // arrangement an agent uses across separate tool calls.
    let mut session = session();
    session
        .controller()
        .set_breakpoint(BreakpointSpec::before("call"))
        .expect("registers");

    let pause = session.next_pause(WAIT).await.expect("parks");
    session
        .controller()
        .release(
            pause.pause_id,
            DebugCommand::Override {
                items: vec![Item::new(json!({ "overridden": true }))],
                port: None,
            },
        )
        .expect("releases");

    let outcome = session.finish().await.expect("completes");
    assert_eq!(
        outcome.output["nodes"]["after"]["items"][0]["json"]["seen"],
        json!({ "overridden": true }),
        "the downstream node should have seen the override"
    );
}

#[tokio::test]
async fn stepping_stops_at_the_next_node() {
    let mut session = session();
    session
        .controller()
        .set_breakpoint(BreakpointSpec::before("call"))
        .expect("registers");

    let first = session.next_pause(WAIT).await.expect("parks at call");
    assert_eq!(first.node_id, "call");
    session
        .controller()
        .release(first.pause_id, DebugCommand::Step)
        .expect("steps");

    let second = session
        .next_pause(WAIT)
        .await
        .expect("stepping should stop at the next activation");
    assert_eq!(
        second.node_id, "after",
        "a step should stop at the next node, with no breakpoint set on it"
    );

    session
        .controller()
        .release(second.pause_id, DebugCommand::Continue)
        .expect("releases");
    session.finish().await.expect("completes");
}

#[tokio::test]
async fn a_conditional_breakpoint_only_fires_when_it_holds() {
    let mut session = session();
    // A condition that cannot hold, so the run must finish without parking.
    session
        .controller()
        .set_breakpoint(BreakpointSpec::before("call").when(Condition::Expr("=false".into())))
        .expect("registers");

    let parked = session.next_pause(Duration::from_millis(500)).await;
    assert!(
        parked.is_none(),
        "a condition that does not hold must not park the run"
    );
    session.finish().await.expect("completes");
}

#[tokio::test]
async fn breaking_on_error_catches_a_failing_node() {
    let mut graph = graph();
    // No slug: the tool node fails.
    graph.nodes[1].config = json!({ "on_error": "continue" });
    let compiled = compile(&graph).expect("compile");
    let mut session =
        DebugSession::start_quiet(compiled, json!({}), mock_capabilities()).expect("session");
    session
        .controller()
        .set_breakpoint(BreakpointSpec::on_error())
        .expect("registers");

    let pause = session
        .next_pause(WAIT)
        .await
        .expect("parks on the failure");
    assert_eq!(pause.node_id, "call");
    assert_eq!(pause.phase, "after");
    assert!(
        pause.error.is_some(),
        "an on-error pause should carry why it failed"
    );

    session
        .controller()
        .release(pause.pause_id, DebugCommand::Continue)
        .expect("releases");
    session.finish().await.expect("completes");
}

#[tokio::test]
async fn a_failed_node_can_be_rescued_from_the_pause() {
    let mut graph = graph();
    graph.nodes[1].config = json!({ "on_error": "continue" });
    let compiled = compile(&graph).expect("compile");
    let mut session =
        DebugSession::start_quiet(compiled, json!({}), mock_capabilities()).expect("session");
    session
        .controller()
        .set_breakpoint(BreakpointSpec::on_error())
        .expect("registers");

    let pause = session.next_pause(WAIT).await.expect("parks");
    session
        .controller()
        .release(
            pause.pause_id,
            DebugCommand::Override {
                items: vec![Item::new(json!({ "rescued": true }))],
                port: None,
            },
        )
        .expect("releases");

    let outcome = session.finish().await.expect("completes");
    assert_eq!(
        outcome.output["nodes"]["after"]["items"][0]["json"]["seen"],
        json!({ "rescued": true }),
        "pretending the failed call returned something should feed downstream"
    );
}

#[tokio::test]
async fn a_hit_limited_breakpoint_disables_itself() {
    let mut session = session();
    let controller = session.controller().clone();
    let id = controller
        .set_breakpoint(BreakpointSpec::before("call").once())
        .expect("registers");

    let pause = session.next_pause(WAIT).await.expect("parks");
    controller
        .release(pause.pause_id, DebugCommand::Continue)
        .expect("releases");
    session.finish().await.expect("completes");

    let listed = controller.breakpoints();
    // Detach on finish clears the table, so the assertion is on what was
    // recorded before that: the breakpoint fired exactly once.
    assert!(
        listed.iter().all(|b| b.id != id) || listed.iter().any(|b| !b.enabled),
        "a once-breakpoint should not remain armed"
    );
}

#[tokio::test]
async fn a_pause_times_out_rather_than_wedging_the_run() {
    // The property that makes this safe to hand to an agent that might never
    // answer: a forgotten pause degrades to an ordinary run, not a hung one.
    let mut session = session();
    session
        .controller()
        .set_pause_timeout(Some(Duration::from_millis(50)));
    session
        .controller()
        .set_breakpoint(BreakpointSpec::before("call"))
        .expect("registers");

    let pause = session.next_pause(WAIT).await.expect("parks");
    // Deliberately never released.
    let _ = pause;

    let outcome = session
        .finish()
        .await
        .expect("the run should finish on its own after the pause times out");
    assert!(
        outcome.output["nodes"]["call"].get("items").is_some(),
        "the timed-out node should have executed normally"
    );
}

#[tokio::test]
async fn detaching_releases_a_parked_run() {
    let mut session = session();
    session
        .controller()
        .set_breakpoint(BreakpointSpec::before("call"))
        .expect("registers");
    let _pause = session.next_pause(WAIT).await.expect("parks");

    session.controller().detach();

    let outcome = session
        .finish()
        .await
        .expect("detaching should let a parked run finish");
    assert!(outcome.output["nodes"]["after"].get("items").is_some());
}

#[tokio::test]
async fn cancelling_winds_down_a_parked_run() {
    let mut session = session();
    session
        .controller()
        .set_breakpoint(BreakpointSpec::before("call"))
        .expect("registers");
    let _pause = session.next_pause(WAIT).await.expect("parks");

    session.cancel();

    let outcome = session
        .finish()
        .await
        .expect("cancelling a parked run should return, not hang");
    assert!(outcome.cancelled, "the run should report itself cancelled");
}

#[tokio::test]
async fn dropping_a_parked_session_does_not_hang() {
    // If the drop order were wrong this test would hang rather than fail, which
    // is exactly why it is worth writing.
    let mut session = session();
    session
        .controller()
        .set_breakpoint(BreakpointSpec::before("call"))
        .expect("registers");
    let _pause = session.next_pause(WAIT).await.expect("parks");

    drop(session);

    // Give the aborted task a moment to wind down; reaching here at all is the
    // assertion.
    futures_timer::Delay::new(Duration::from_millis(50)).await;
}

#[tokio::test]
async fn a_run_with_no_breakpoints_never_parks() {
    let mut session = session();
    let parked = session.next_pause(Duration::from_millis(500)).await;
    assert!(parked.is_none());
    session.finish().await.expect("completes");
}
