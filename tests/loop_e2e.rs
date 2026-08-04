#![cfg(feature = "mock")]
//! End-to-end tests for bounded loops: the `loop` node kind and the back-edge
//! lowering that makes any cycle iterate instead of deadlocking.
//!
//! These drive real graphs through the real engine on the deterministic mock
//! capabilities. The loop body is built from pure control-flow nodes
//! (`output_parser` passthroughs, `transform`) so no capability needs swapping
//! and the assertions are about routing, not about what a host returned.
//!
//! **Every test wraps its run in a `tokio::time::timeout`.** A loop bug is
//! exactly the kind that hangs rather than fails, and a hung test takes the
//! whole suite with it — the guard turns that into a named failure.
//!
//! Gated behind the `mock` cargo feature so plain `cargo test` skips it while
//! `cargo test --all-features` runs it.

use std::time::Duration;

use serde_json::{Value, json};

use tinyflows::caps::mock::mock_capabilities;
use tinyflows::compiler::compile;
use tinyflows::engine::run;
use tinyflows::error::EngineError;
use tinyflows::error::ValidationError;
use tinyflows::model::{Edge, Node, NodeKind, WorkflowGraph};

/// How long any single run in this file may take before it is called a hang.
const GUARD: Duration = Duration::from_secs(10);

/// Builds a node with the given id, kind, and config (no ports, no position).
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

/// Builds a `main` -> `main` edge from `from` to `to`.
fn edge(from: &str, to: &str) -> Edge {
    Edge {
        from_node: from.to_string(),
        from_port: "main".to_string(),
        to_node: to.to_string(),
        to_port: "main".to_string(),
    }
}

/// Builds an edge leaving `from` on a named port (`body`, `done`, …).
fn port_edge(from: &str, from_port: &str, to: &str) -> Edge {
    Edge {
        from_node: from.to_string(),
        from_port: from_port.to_string(),
        to_node: to.to_string(),
        to_port: "main".to_string(),
    }
}

/// The canonical shape: `t -> l --body--> work --> (back to l)`, `l --done--> out`.
///
/// `loop_config` is the `loop` node's config, which is what each test varies.
fn loop_graph(loop_config: Value) -> WorkflowGraph {
    WorkflowGraph {
        name: "bounded_loop".to_string(),
        nodes: vec![
            node("t", NodeKind::Trigger, Value::Null),
            node("l", NodeKind::Loop, loop_config),
            node("work", NodeKind::OutputParser, Value::Null),
            node("out", NodeKind::OutputParser, Value::Null),
        ],
        edges: vec![
            edge("t", "l"),
            port_edge("l", "body", "work"),
            edge("work", "l"),
            port_edge("l", "done", "out"),
        ],
        ..Default::default()
    }
}

/// Runs a graph under the hang guard, returning the engine's result.
async fn run_guarded(
    graph: &WorkflowGraph,
) -> tinyflows::error::Result<tinyflows::engine::RunOutcome> {
    let caps = mock_capabilities();
    let compiled = compile(graph).expect("compile");
    match tokio::time::timeout(GUARD, run(&compiled, json!({}), &caps)).await {
        Err(_elapsed) => panic!("run hung past {GUARD:?} — the loop did not terminate"),
        Ok(inner) => inner,
    }
}

#[tokio::test]
async fn a_loop_runs_its_body_and_exits_cleanly_at_the_cap() {
    let outcome = run_guarded(&loop_graph(
        json!({ "max_iterations": 3, "on_exceeded": "continue" }),
    ))
    .await
    .expect("the loop should finish rather than error");

    // The body ran, and the run left through `done` into the downstream node.
    assert_eq!(
        outcome.output["nodes"]["l"]["iteration"], 3,
        "the loop should have consumed exactly its three iterations"
    );
    assert_eq!(
        outcome.output["nodes"]["l"]["port"], "done",
        "the final activation exits on `done`"
    );
    assert!(
        !outcome.output["nodes"]["out"].is_null(),
        "the downstream node must run once the loop is done"
    );
}

#[tokio::test]
async fn the_default_policy_fails_the_run_at_the_cap() {
    let err = run_guarded(&loop_graph(json!({ "max_iterations": 2 })))
        .await
        .expect_err("on_exceeded defaults to `error`");
    match err {
        EngineError::LoopLimit { node, limit } => {
            assert_eq!(node, "l", "the error names the loop that ran away");
            assert_eq!(limit, 2);
        }
        other => panic!("expected LoopLimit, got: {other:?}"),
    }
}

#[tokio::test]
async fn a_falsey_condition_exits_before_the_cap_is_reached() {
    // The condition never holds, so the loop leaves on `done` immediately and
    // the body never runs at all.
    let outcome = run_guarded(&loop_graph(json!({
        "max_iterations": 50,
        "condition": "=item.never_set"
    })))
    .await
    .expect("an early exit is not an error");

    assert_eq!(outcome.output["nodes"]["l"]["iteration"], 0);
    assert!(
        outcome.output["nodes"]["work"].is_null(),
        "the body must not run when the condition is false on the first pass"
    );
    assert!(
        !outcome.output["nodes"]["out"].is_null(),
        "the run still completes through `done`"
    );
}

/// The regression that motivated the feature: a cycle closing on a *mid-graph*
/// node. Such a node has two predecessors, and the engine used to lower its
/// incoming edges as a fan-in merge barrier — which waited forever for the
/// back-edge that could only arrive after the node itself ran. The run settled
/// as `Ok` with only the trigger having executed.
#[tokio::test]
async fn a_plain_back_edge_onto_a_mid_graph_node_iterates() {
    let graph = WorkflowGraph {
        name: "plain_cycle".to_string(),
        nodes: vec![
            node("t", NodeKind::Trigger, json!({ "recursion_limit": 6 })),
            node("a", NodeKind::OutputParser, Value::Null),
            node("b", NodeKind::OutputParser, Value::Null),
        ],
        edges: vec![edge("t", "a"), edge("a", "b"), edge("b", "a")],
        ..Default::default()
    };

    // With no loop node to bound it, the graph-wide recursion limit is what
    // stops the run — and reaching it proves the cycle actually iterated.
    let err = run_guarded(&graph)
        .await
        .expect_err("an unbounded cycle must hit the recursion limit");
    assert!(
        !matches!(err, EngineError::Validation(_)),
        "the graph is legal; it should fail at run time, not validation: {err:?}"
    );
}

/// A loop head may not itself be a fan-in, and validation says so rather than
/// letting the graph run once and stop. The barrier tinyagents installs for a
/// node's forward predecessors is per-node, so it swallows the re-entry the
/// back-edge delivers — the loop would silently iterate exactly once.
#[tokio::test]
async fn a_loop_head_that_is_also_a_fan_in_is_refused() {
    let graph = WorkflowGraph {
        name: "fan_in_loop".to_string(),
        nodes: vec![
            node("t", NodeKind::Trigger, Value::Null),
            node("left", NodeKind::OutputParser, Value::Null),
            node("right", NodeKind::OutputParser, Value::Null),
            node(
                "l",
                NodeKind::Loop,
                json!({ "max_iterations": 2, "on_exceeded": "continue" }),
            ),
            node("work", NodeKind::OutputParser, Value::Null),
            node("out", NodeKind::OutputParser, Value::Null),
        ],
        edges: vec![
            edge("t", "left"),
            edge("t", "right"),
            edge("left", "l"),
            edge("right", "l"),
            port_edge("l", "body", "work"),
            edge("work", "l"),
            port_edge("l", "done", "out"),
        ],
        ..Default::default()
    };

    let errors = tinyflows::validate::validate_all(&graph);
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, ValidationError::IllegalCycle(id) if id == "l")),
        "a fan-in loop head should be refused, got: {errors:?}"
    );
}

/// The fix for the case above: join the arms *before* the loop head, so the
/// head keeps a single forward predecessor and the merge sits outside the
/// cycle. This is the shape the validation error points authors at.
#[tokio::test]
async fn a_fan_in_joined_before_the_loop_head_iterates() {
    let graph = WorkflowGraph {
        name: "fan_in_then_loop".to_string(),
        nodes: vec![
            node("t", NodeKind::Trigger, Value::Null),
            node("left", NodeKind::OutputParser, Value::Null),
            node("right", NodeKind::OutputParser, Value::Null),
            node("j", NodeKind::Merge, Value::Null),
            node(
                "l",
                NodeKind::Loop,
                json!({ "max_iterations": 2, "on_exceeded": "continue" }),
            ),
            node("work", NodeKind::OutputParser, Value::Null),
            node("out", NodeKind::OutputParser, Value::Null),
        ],
        edges: vec![
            edge("t", "left"),
            edge("t", "right"),
            edge("left", "j"),
            edge("right", "j"),
            edge("j", "l"),
            port_edge("l", "body", "work"),
            edge("work", "l"),
            port_edge("l", "done", "out"),
        ],
        ..Default::default()
    };

    assert!(
        tinyflows::validate::validate_all(&graph).is_empty(),
        "joining before the head is the supported shape"
    );
    let outcome = run_guarded(&graph).await.expect("the loop should finish");
    assert!(
        !outcome.output["nodes"]["left"].is_null() && !outcome.output["nodes"]["right"].is_null(),
        "both arms of the fan-in must run"
    );
    assert_eq!(outcome.output["nodes"]["l"]["iteration"], 2);
    assert!(!outcome.output["nodes"]["out"].is_null());
}

/// A `merge` *inside* the loop body is the other shape that cannot iterate: its
/// barrier waits for every predecessor, and on the second pass only the looping
/// arm arrives.
#[tokio::test]
async fn a_merge_inside_the_loop_body_is_refused() {
    let graph = WorkflowGraph {
        name: "merge_in_body".to_string(),
        nodes: vec![
            node("t", NodeKind::Trigger, Value::Null),
            node(
                "l",
                NodeKind::Loop,
                json!({ "max_iterations": 2, "on_exceeded": "continue" }),
            ),
            node("m", NodeKind::Merge, Value::Null),
            node("out", NodeKind::OutputParser, Value::Null),
        ],
        edges: vec![
            edge("t", "l"),
            port_edge("l", "body", "m"),
            edge("m", "l"),
            port_edge("l", "done", "out"),
        ],
        ..Default::default()
    };

    let errors = tinyflows::validate::validate_all(&graph);
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, ValidationError::IllegalCycle(id) if id == "m")),
        "a merge on the cycle should be refused, got: {errors:?}"
    );
}

/// An unbounded cycle — no `loop` node counting passes and no
/// `recursion_limit` on the trigger — is refused, so the bound is an authoring
/// decision rather than whatever the host's wall clock happens to be.
#[tokio::test]
async fn an_unbounded_cycle_is_refused() {
    let graph = WorkflowGraph {
        name: "unbounded".to_string(),
        nodes: vec![
            node("t", NodeKind::Trigger, Value::Null),
            node("a", NodeKind::OutputParser, Value::Null),
            node("b", NodeKind::OutputParser, Value::Null),
        ],
        edges: vec![edge("t", "a"), edge("a", "b"), edge("b", "a")],
        ..Default::default()
    };

    let errors = tinyflows::validate::validate_all(&graph);
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, ValidationError::IllegalCycle(_))),
        "an uncapped cycle should be refused, got: {errors:?}"
    );
}

/// A loop can read its own pass number from anywhere in the graph, which is
/// what makes an iteration-aware body possible.
#[tokio::test]
async fn the_iteration_count_is_readable_from_an_expression() {
    let graph = WorkflowGraph {
        name: "iteration_expr".to_string(),
        nodes: vec![
            node("t", NodeKind::Trigger, Value::Null),
            node(
                "l",
                NodeKind::Loop,
                json!({ "max_iterations": 3, "on_exceeded": "continue" }),
            ),
            node(
                "work",
                NodeKind::Transform,
                json!({ "set": { "pass": "=nodes.l.iteration" } }),
            ),
            node("out", NodeKind::OutputParser, Value::Null),
        ],
        edges: vec![
            edge("t", "l"),
            port_edge("l", "body", "work"),
            edge("work", "l"),
            port_edge("l", "done", "out"),
        ],
        ..Default::default()
    };

    let outcome = run_guarded(&graph).await.expect("the loop should finish");
    // The body's last pass saw the third iteration, so the counter reached the
    // body rather than resolving null.
    assert_eq!(
        outcome.output["nodes"]["work"]["items"][0]["json"]["pass"], 3,
        "the body should see the current pass number"
    );
}

/// `max_node_visits` is the backstop under a `loop` node: it bounds a cycle
/// nobody declared a loop node for, and unlike `recursion_limit` the failure
/// names the node that ran away rather than just reporting too many steps.
#[tokio::test]
async fn max_node_visits_bounds_a_cycle_and_names_the_node() {
    let graph = WorkflowGraph {
        name: "visit_capped".to_string(),
        nodes: vec![
            node(
                "t",
                NodeKind::Trigger,
                json!({ "recursion_limit": 500, "max_node_visits": 3 }),
            ),
            node("a", NodeKind::OutputParser, Value::Null),
            node("b", NodeKind::OutputParser, Value::Null),
        ],
        edges: vec![edge("t", "a"), edge("a", "b"), edge("b", "a")],
        ..Default::default()
    };

    let err = run_guarded(&graph)
        .await
        .expect_err("the visit cap must stop the cycle");
    let message = err.to_string();
    assert!(
        message.contains('a') && message.to_lowercase().contains("visit"),
        "the failure should name the node and its visit cap, got: {message}"
    );
}
