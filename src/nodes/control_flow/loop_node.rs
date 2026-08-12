//! The `loop` node: a bounded loop head.
//!
//! A loop is written as a cycle whose head is this node:
//!
//! ```text
//! trigger -> loop --body--> work -> more_work --+
//!              ^                                |
//!              +--------------------------------+
//!              |
//!              +--done--> output
//! ```
//!
//! The closing edge (`more_work -> loop`) is a back-edge, which the engine
//! detects and lowers as a plain re-entry rather than a fan-in merge barrier —
//! see [`crate::engine`]. This node then decides, on each activation, whether to
//! send its input round the `body` again or let it out through `done`.
//!
//! **Why the counter lives in run state.** The iteration count is written to
//! this node's own slot via [`NodeOutput::meta`], so it is part of the state the
//! engine checkpoints. A loop therefore resumes mid-iteration with its count
//! intact, and the count is addressable from any expression in the graph as
//! `=nodes.<loop id>.iteration`. Holding it in the executor instead would lose
//! it on every pause, and threading it through the items would lose it to the
//! first node in the body that reshapes them.

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::error::{EngineError, Result};
use crate::expr;
use crate::nodes::{NodeContext, NodeExecutor, NodeOutput, expr_scope};

/// The default `max_iterations` for a `loop` node that does not declare one.
///
/// Deliberately finite. A loop head with no cap is the runaway case this node
/// exists to prevent, and the graph-wide `recursion_limit` backstop reports
/// only that *the run* looped, never which loop was responsible.
pub const DEFAULT_MAX_ITERATIONS: u64 = 25;

/// Bounded loop head: routes to `body` until its cap or condition says stop,
/// then to `done`.
#[derive(Debug, Default, Clone)]
pub struct LoopNode;

/// What a loop does when it reaches `max_iterations`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OnExceeded {
    /// Fail the run with [`EngineError::LoopLimit`] (the default).
    Error,
    /// Stop looping and emit on `done`, so downstream still runs with whatever
    /// the last iteration produced.
    Continue,
}

impl OnExceeded {
    /// Reads the policy from a node's `config.on_exceeded`.
    ///
    /// Defaults to [`Self::Error`] — both when unset and when the value is not
    /// one of the two known strings. An unrecognised value is refused by
    /// [`crate::validate`] before a run ever starts, so this fallback only
    /// covers a graph that bypassed validation, where failing loudly beats
    /// silently looping on.
    fn from_config(config: &Value) -> Self {
        match config.get("on_exceeded").and_then(Value::as_str) {
            Some("continue") => Self::Continue,
            _ => Self::Error,
        }
    }
}

/// The iteration count this node recorded on its previous activation, or 0 the
/// first time through.
fn current_iteration(ctx: &NodeContext) -> u64 {
    ctx.nodes
        .get(&ctx.node.id)
        .and_then(|slot| slot.get("iteration"))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

/// Whether the node's optional `config.condition` expression is truthy.
///
/// Returns `true` when no condition is configured, so a loop bounded only by
/// `max_iterations` always continues. The expression is resolved against the
/// node's normal scope, so it can read the current pass's items (`=item.done`)
/// as well as the counter itself (`=nodes.<id>.iteration`).
fn condition_holds(ctx: &NodeContext) -> bool {
    let Some(condition) = ctx.node.config.get("condition") else {
        return true;
    };
    let resolved = if condition.as_str().is_some_and(expr::is_expression) {
        expr::evaluate(condition, &expr_scope(ctx))
    } else {
        condition.clone()
    };
    is_truthy(&resolved)
}

/// Truthiness predicate, matching the `condition` node's: `null`, `false`, `0`,
/// `""`, `[]`, and `{}` are falsey; everything else is truthy.
fn is_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

#[async_trait]
impl NodeExecutor for LoopNode {
    async fn execute(&self, ctx: NodeContext<'_>) -> Result<NodeOutput> {
        let iteration = current_iteration(&ctx);
        let max_iterations = ctx
            .node
            .config
            .get("max_iterations")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_MAX_ITERATIONS);
        let items = ctx.input.to_vec();
        // The count is recorded on every path, including the exits, so a host
        // reading the finished run can see how many passes actually happened.
        let done = |iteration: u64| {
            Ok(NodeOutput::routed(items.clone(), "done")
                .with_meta(json!({ "iteration": iteration })))
        };

        // The condition is checked before the cap so a loop that finishes early
        // on its own terms never trips the limit, and checked before the
        // iteration is consumed so `condition: false` exits without a pass.
        if !condition_holds(&ctx) {
            return done(iteration);
        }

        if iteration >= max_iterations {
            return match OnExceeded::from_config(&ctx.node.config) {
                OnExceeded::Error => Err(EngineError::LoopLimit {
                    node: ctx.node.id.clone(),
                    limit: max_iterations,
                }),
                OnExceeded::Continue => done(iteration),
            };
        }

        Ok(NodeOutput::routed(items, "body").with_meta(json!({ "iteration": iteration + 1 })))
    }
}

#[cfg(test)]
mod tests {
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
                token: crate::engine::CancellationToken::new(),
            })
            .await
    }

    #[tokio::test]
    async fn first_pass_routes_to_body_and_starts_the_count() {
        let out = run_with(json!({ "max_iterations": 3 }), Value::Null)
            .await
            .expect("execute");
        assert_eq!(out.port.as_deref(), Some("body"));
        assert_eq!(out.meta, Some(json!({ "iteration": 1 })));
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
        assert_eq!(out.meta, Some(json!({ "iteration": 3 })));
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
        assert_eq!(out.meta, Some(json!({ "iteration": 3 })));
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
        assert_eq!(out.meta, Some(json!({ "iteration": 0 })));
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
}
