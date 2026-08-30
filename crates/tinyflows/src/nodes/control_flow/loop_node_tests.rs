use super::*;
use crate::caps::mock::mock_capabilities;
use crate::data::Item;
use crate::model::{Node, NodeKind};

fn loop_node(config: Value) -> Node {
    Node {
        id: "l".to_string(),
        kind: NodeKind::Loop,
        type_version: 1,
        name: "l".to_string(),
        config,
        ports: Vec::new(),
        position: None,
    }
}

/// Runs the executor with an explicit `nodes` state slice, which is how the
/// iteration count is fed back in between activations.
async fn run_with(config: Value, nodes: Value) -> Result<NodeOutput> {
    let caps = mock_capabilities();
    let node = loop_node(config);
    let input = vec![Item::new(json!({ "x": 1 }))];
    LoopNode
        .execute(NodeContext {
            node: &node,
            input: &input,
            run: &Value::Null,
            nodes: &nodes,
            caps: &caps,
            agents: &[],
            observer: &crate::observability::NoopObserver,
            token: crate::engine::CancellationToken::new(),
            lane: None,
            resume: None,
            step: 0,
        })
        .await
}

#[tokio::test]
async fn first_pass_routes_to_body_and_starts_the_count() {
    let out = run_with(json!({ "max_iterations": 3 }), Value::Null)
        .await
        .expect("execute");
    assert_eq!(out.port.as_deref(), Some("body"));
    assert_eq!(
        out.meta.as_ref().and_then(|m| m.get("iteration")),
        Some(&json!(1))
    );
    assert_eq!(out.items.len(), 1, "input passes through to the body");
}

#[tokio::test]
async fn a_pass_below_the_cap_keeps_looping_and_increments() {
    let out = run_with(
        json!({ "max_iterations": 3 }),
        json!({ "l": { "iteration": 2 } }),
    )
    .await
    .expect("execute");
    assert_eq!(out.port.as_deref(), Some("body"));
    assert_eq!(
        out.meta.as_ref().and_then(|m| m.get("iteration")),
        Some(&json!(3))
    );
}

#[tokio::test]
async fn reaching_the_cap_errors_by_default() {
    let err = run_with(
        json!({ "max_iterations": 3 }),
        json!({ "l": { "iteration": 3 } }),
    )
    .await
    .expect_err("should refuse to loop past the cap");
    match err {
        EngineError::LoopLimit { node, limit } => {
            assert_eq!(node, "l");
            assert_eq!(limit, 3);
        }
        other => panic!("expected LoopLimit, got: {other:?}"),
    }
}

#[tokio::test]
async fn on_exceeded_continue_exits_through_done() {
    let out = run_with(
        json!({ "max_iterations": 3, "on_exceeded": "continue" }),
        json!({ "l": { "iteration": 3 } }),
    )
    .await
    .expect("execute");
    assert_eq!(out.port.as_deref(), Some("done"));
    assert_eq!(
        out.meta.as_ref().and_then(|m| m.get("iteration")),
        Some(&json!(3))
    );
    assert_eq!(out.items.len(), 1, "the last pass's items reach downstream");
}

/// An unrecognised policy must not be read as `continue` — failing closed is
/// what keeps a typo from silently uncapping a loop.
#[tokio::test]
async fn an_unknown_on_exceeded_value_falls_back_to_error() {
    let err = run_with(
        json!({ "max_iterations": 1, "on_exceeded": "carry-on" }),
        json!({ "l": { "iteration": 1 } }),
    )
    .await
    .expect_err("unknown policy should fail closed");
    assert!(matches!(err, EngineError::LoopLimit { .. }));
}

#[tokio::test]
async fn a_falsey_condition_exits_before_consuming_an_iteration() {
    let out = run_with(
        json!({ "max_iterations": 10, "condition": "=item.keep_going" }),
        Value::Null,
    )
    .await
    .expect("execute");
    assert_eq!(
        out.port.as_deref(),
        Some("done"),
        "no `keep_going` field resolves null, which is falsey"
    );
    assert_eq!(
        out.meta.as_ref().and_then(|m| m.get("iteration")),
        Some(&json!(0))
    );
}

#[tokio::test]
async fn a_truthy_condition_keeps_looping() {
    let out = run_with(
        json!({ "max_iterations": 10, "condition": "=item.x" }),
        Value::Null,
    )
    .await
    .expect("execute");
    assert_eq!(out.port.as_deref(), Some("body"));
}

/// The condition wins over the cap, so a loop that finishes on its own terms
/// exits cleanly rather than erroring on the limit.
#[tokio::test]
async fn a_falsey_condition_beats_an_exhausted_cap() {
    let out = run_with(
        json!({ "max_iterations": 1, "condition": false }),
        json!({ "l": { "iteration": 1 } }),
    )
    .await
    .expect("condition should exit before the cap is consulted");
    assert_eq!(out.port.as_deref(), Some("done"));
}

#[tokio::test]
async fn an_undeclared_cap_falls_back_to_the_default() {
    let err = run_with(
        json!({}),
        json!({ "l": { "iteration": DEFAULT_MAX_ITERATIONS } }),
    )
    .await
    .expect_err("the default cap still bounds the loop");
    assert!(matches!(
        err,
        EngineError::LoopLimit { limit, .. } if limit == DEFAULT_MAX_ITERATIONS
    ));
}
