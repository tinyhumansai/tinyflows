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
//! **Why the counter and the accumulator live in run state.** Both are written
//! to this node's own slot via [`NodeOutput::meta`], so they are part of the
//! state the engine checkpoints. A loop therefore resumes mid-iteration with
//! both intact, and both are addressable from any expression in the graph as
//! `=nodes.<loop id>.iteration` / `=nodes.<loop id>.state`. Holding them in the
//! executor instead would lose them on every pause, and threading them through
//! the items would lose them to the first node in the body that reshapes them.
//!
//! **The accumulator is a fold.** `state.init` seeds it once; `state.update`
//! folds each pass's body output into it, so `acc_next = f(acc_prev, output)`.
//! This node is the *sole writer* of that slot, which is what keeps it simple:
//! no reducer collision, no question of which branch wrote last, no interaction
//! with the staleness stamping that loop re-entry uses.
//!
//! Because the reducer merges objects key-by-key, an accumulator written
//! plainly could only ever *gain* keys — an error recorded on pass 1 would
//! haunt every later pass. The accumulator is therefore written through
//! [`crate::engine::replace`], which assigns the slot wholesale.
//!
//! **The fold is at-least-once.** If an activation is replayed after a resume,
//! the update applies twice. This is not new — `iteration + 1` has always had
//! the same property — but an accumulator makes it visible, as a duplicated
//! append. Fixing it properly means stamping the fold with the super-step that
//! produced it and skipping a repeat, which should be done for the counter and
//! the accumulator together. Until then, an idempotent `update` (assign the
//! next value rather than appending to the previous one) is immune.

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

/// What the exit ports carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EmitMode {
    /// The last pass's items (the default, and what a loop without an
    /// accumulator has always emitted).
    Items,
    /// One item holding the accumulator.
    State,
    /// The last pass's items, with the accumulator appended.
    Both,
}

impl EmitMode {
    fn from_config(config: &Value) -> Self {
        match config.get("emit").and_then(Value::as_str) {
            Some("state") => Self::State,
            Some("both") => Self::Both,
            _ => Self::Items,
        }
    }

    /// Builds the items an exit port carries.
    fn items(self, items: &[crate::data::Item], state: &Value) -> Vec<crate::data::Item> {
        match self {
            Self::Items => items.to_vec(),
            Self::State => vec![crate::data::Item::new(state.clone())],
            Self::Both => {
                let mut out = items.to_vec();
                out.push(crate::data::Item::new(state.clone()));
                out
            }
        }
    }
}

/// Whether an `until` exit should leave on its own `success` port.
fn success_port(config: &Value) -> bool {
    config
        .get("success_port")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// The accumulator this node recorded on its previous activation.
fn current_state(ctx: &NodeContext) -> Option<Value> {
    ctx.nodes
        .get(&ctx.node.id)
        .and_then(|slot| slot.get("state"))
        .cloned()
}

/// The accumulator's starting value, from `config.state.init`.
///
/// Resolved on the seeding activation only, so an expression in `init` sees the
/// run as it was when the loop began rather than being re-evaluated per pass.
fn initial_state(ctx: &NodeContext) -> Result<Value> {
    let Some(init) = ctx
        .node
        .config
        .get("state")
        .and_then(|state| state.get("init"))
    else {
        return Ok(Value::Null);
    };
    Ok(if init.as_str().is_some_and(expr::is_expression) {
        expr::evaluate(init, &expr_scope(ctx))
    } else {
        init.clone()
    })
}

/// Applies `config.state.update` to the accumulator, given the body's output.
///
/// The scope is the node's usual one plus a `state` key holding the *previous*
/// accumulator, so an update reads `state` and the body's items together:
/// `acc_next = f(acc_prev, body_output)`.
///
/// Two spellings, both supported because they suit different authors: a single
/// jq program folding the whole accumulator, or an object of per-key
/// expressions (mirroring `transform.set`), which is what someone who does not
/// write jq will reach for.
fn fold_state(ctx: &NodeContext) -> Result<Value> {
    let previous = match current_state(ctx) {
        Some(state) => state,
        // No recorded accumulator: this loop either declares none, or is being
        // re-entered after its slot was never written. Fall back to `init`
        // rather than folding into null.
        None => initial_state(ctx)?,
    };
    let Some(update) = ctx
        .node
        .config
        .get("state")
        .and_then(|state| state.get("update"))
    else {
        return Ok(previous);
    };

    let mut scope = expr_scope(ctx);
    if let Some(map) = scope.as_object_mut() {
        map.insert("state".to_string(), previous.clone());
    }

    match update {
        // Object form: each key is resolved independently and merged over the
        // previous accumulator, so an update naming one key leaves the rest.
        Value::Object(fields) => {
            let mut next = previous;
            let entries: Vec<(String, Value)> = fields
                .iter()
                .map(|(key, raw)| {
                    let value = if raw.as_str().is_some_and(expr::is_expression) {
                        expr::evaluate(raw, &scope)
                    } else {
                        raw.clone()
                    };
                    (key.clone(), value)
                })
                .collect();
            if !next.is_object() {
                next = json!({});
            }
            if let Some(map) = next.as_object_mut() {
                for (key, value) in entries {
                    map.insert(key, value);
                }
            }
            Ok(next)
        }
        // Program form: one jq expression producing the whole next accumulator.
        raw if raw.as_str().is_some_and(expr::is_expression) => Ok(expr::evaluate(raw, &scope)),
        // A literal: the accumulator simply becomes it.
        raw => Ok(raw.clone()),
    }
}

/// Whether the node's optional `config.until` expression is truthy against the
/// **post-fold** accumulator.
///
/// Opposite polarity to `condition`, deliberately: `condition` says *keep going
/// while*, `until` says *stop when*. Both are supported because a real loop
/// often has both a work-remaining test and a success test.
fn until_holds(ctx: &NodeContext, state: &Value) -> bool {
    let Some(until) = ctx.node.config.get("until") else {
        return false;
    };
    let mut scope = expr_scope(ctx);
    if let Some(map) = scope.as_object_mut() {
        map.insert("state".to_string(), state.clone());
    }
    let resolved = if until.as_str().is_some_and(expr::is_expression) {
        expr::evaluate(until, &scope)
    } else {
        until.clone()
    };
    is_truthy(&resolved)
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

        // Fold the body's output into the accumulator, before any exit is
        // considered — so `until` tests the state *including* the pass that just
        // finished, which is what "stop when the check passes" has to mean.
        //
        // Only on re-entry: on the seeding activation the body has not run, so
        // there is nothing to fold and `init` stands.
        let state = if iteration > 0 {
            fold_state(&ctx)?
        } else {
            initial_state(&ctx)?
        };
        let emit_mode = EmitMode::from_config(&ctx.node.config);

        // Every path records the count and the accumulator, so a host reading a
        // finished run sees both how many passes happened and what they built.
        let exit = |iteration: u64, reason: &str, port: &str| {
            Ok(
                NodeOutput::routed(emit_mode.items(&items, &state), port).with_meta(json!({
                    "iteration": iteration,
                    "state": crate::engine::replace(state.clone()),
                    "exit_reason": reason,
                })),
            )
        };

        // `until` is the accumulator's own exit: truthy means the check passed.
        // Checked first because converging is a better outcome than either
        // running out of work or running out of tries.
        if until_holds(&ctx, &state) {
            let port = if success_port(&ctx.node.config) {
                "success"
            } else {
                "done"
            };
            return exit(iteration, "until", port);
        }

        // The condition is checked before the cap so a loop that finishes early
        // on its own terms never trips the limit, and checked before the
        // iteration is consumed so `condition: false` exits without a pass.
        if !condition_holds(&ctx) {
            return exit(iteration, "condition", "done");
        }

        if iteration >= max_iterations {
            return match OnExceeded::from_config(&ctx.node.config) {
                OnExceeded::Error => Err(EngineError::LoopLimit {
                    node: ctx.node.id.clone(),
                    limit: max_iterations,
                }),
                OnExceeded::Continue => exit(iteration, "max_iterations", "done"),
            };
        }

        Ok(NodeOutput::routed(items, "body").with_meta(json!({
            "iteration": iteration + 1,
            "state": crate::engine::replace(state),
        })))
    }
}

#[cfg(test)]
#[path = "loop_node_tests.rs"]
mod tests;
