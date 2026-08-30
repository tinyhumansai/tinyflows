//! The `gate` node: wait for spawned work, on a policy.
//!
//! A gate collects the tickets a [`spawn`](super::spawn) node produced and emits
//! their results. What makes it more than a barrier is the release policy: it
//! can proceed on the first result, on a quorum, or on whatever arrived before
//! its deadline, rather than only on all of them (see [`crate::nodes::release`]).
//!
//! # How waiting works in a super-step engine
//!
//! There is no "block here until the callback comes". A gate is activated,
//! looks at the world, and either proceeds or asks to be run again — via
//! [`NodeControl::Reenter`], which commits its notes and re-activates it in the
//! next super-step. Waiting is therefore counted in **polls**, and every poll
//! costs a super-step and a node visit against the run's budgets. That is why
//! the poll count is bounded here rather than left to the run-level backstop:
//! the backstop cannot say which node span, and a gate that spun forever would
//! look exactly like a runaway loop.
//!
//! For a wait measured in minutes rather than milliseconds, `wait_mode:
//! "suspend"` interrupts the run instead, so nothing is burned while waiting and
//! the host resumes it when the work lands.

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::caps::TaskState;
use crate::data::Item;
use crate::error::{EngineError, Result};
use crate::nodes::release::{Release, ReleasePolicy};
use crate::nodes::{NodeContext, NodeExecutor, NodeOutput};

/// Default gap between polls, in milliseconds.
const DEFAULT_POLL_INTERVAL_MS: u64 = 250;

/// Default ceiling on polls before the wait budget is called spent.
///
/// Finite on purpose. An unbounded gate is the runaway case this bound exists
/// to prevent, and the run-level backstop reports only that the *run* spun,
/// never which gate was responsible.
const DEFAULT_MAX_POLLS: u64 = 200;

/// The slot key a gate records its poll count under, so the count survives a
/// checkpoint the same way a `loop` node's iteration does.
const POLLS_KEY: &str = "polls";

/// How a gate waits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitMode {
    /// Re-activate and look again. Cheap for short waits; costs a super-step
    /// per poll.
    Poll,
    /// Interrupt the run and let the host resume it when the work lands. Right
    /// for waits long enough that burning super-steps would be absurd.
    Suspend,
}

impl WaitMode {
    fn from_config(config: &Value) -> Self {
        match config.get("wait_mode").and_then(Value::as_str) {
            Some("suspend") => Self::Suspend,
            _ => Self::Poll,
        }
    }
}

/// What a gate does when its wait budget runs out and the policy will not
/// settle for a partial result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OnTimeout {
    /// Emit whatever arrived.
    Partial,
    /// Fail the node.
    Error,
    /// Route to the `timeout` port.
    Route,
}

impl OnTimeout {
    fn from_config(config: &Value) -> Self {
        match config.get("on_timeout").and_then(Value::as_str) {
            Some("partial") => Self::Partial,
            Some("route") => Self::Route,
            _ => Self::Error,
        }
    }
}

/// Collects spawned work and emits it once its release policy is satisfied.
#[derive(Debug, Default, Clone)]
pub struct GateNode;

/// One thing the gate is waiting on.
struct Awaited {
    /// The ticket, or `None` for an inline-degraded spawn that is already done.
    ticket: Option<String>,
    /// A result already in hand (inline spawn, or delivered by a resume).
    settled: Option<TaskState>,
}

/// The tickets this gate waits on, in the order they will be emitted.
///
/// Two spellings: `from` names upstream `spawn` nodes and reads their tickets
/// out of the run state, and `tickets` is an expression yielding ticket ids for
/// a graph that carries them some other way. `from` is the common case, and the
/// one that keeps a workflow readable — it names nodes, not strings.
fn awaited(ctx: &NodeContext<'_>) -> Result<Vec<Awaited>> {
    let mut awaited = Vec::new();

    if let Some(from) = ctx.node.config.get("from").and_then(Value::as_array) {
        for source in from.iter().filter_map(Value::as_str) {
            let items = ctx
                .nodes
                .get(source)
                .and_then(|slot| slot.get("items"))
                .and_then(Value::as_array);
            let Some(items) = items else {
                // The spawn has not run yet. Not an error: on the gate's first
                // activation its upstream may still be in flight.
                continue;
            };
            for item in items {
                let json = item.get("json").unwrap_or(&Value::Null);
                awaited.push(Awaited {
                    ticket: json
                        .get(super::spawn::TICKET_KEY)
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    settled: super::spawn::inline_result(json),
                });
            }
        }
        return Ok(awaited);
    }

    // Expression form: resolve to a ticket id or an array of them.
    let Some(raw) = ctx.node.config.get("tickets") else {
        return Err(EngineError::Capability(format!(
            "gate node {:?}: needs `from` (spawn node ids) or `tickets` (an expression)",
            ctx.node.id
        )));
    };
    let resolved = if raw.as_str().is_some_and(crate::expr::is_expression) {
        crate::expr::evaluate(raw, &crate::nodes::expr_scope(ctx))
    } else {
        raw.clone()
    };
    let ids: Vec<&Value> = match &resolved {
        Value::Array(values) => values.iter().collect(),
        Value::Null => Vec::new(),
        single => vec![single],
    };
    for id in ids {
        if let Some(ticket) = id.as_str() {
            awaited.push(Awaited {
                ticket: Some(ticket.to_string()),
                settled: None,
            });
        }
    }
    Ok(awaited)
}

/// Results delivered out of band by a resume, keyed by ticket.
fn delivered(ctx: &NodeContext<'_>) -> serde_json::Map<String, Value> {
    ctx.resume
        .as_ref()
        .and_then(|value| value.get("delivered"))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

/// Reads a positive integer config field, falling back to `default`.
fn positive_u64(config: &Value, key: &str, default: u64) -> u64 {
    config
        .get(key)
        .and_then(Value::as_u64)
        .filter(|n| *n > 0)
        .unwrap_or(default)
}

#[async_trait]
impl NodeExecutor for GateNode {
    async fn execute(&self, ctx: NodeContext<'_>) -> Result<NodeOutput> {
        let config = &ctx.node.config;
        let policy = ReleasePolicy::from_config(config, &ctx.node.id)?;
        let awaiting = awaited(&ctx)?;
        let expected = awaiting.len();

        // Poll count carried in this node's own slot, so it survives a
        // checkpoint — the same reason a `loop` node keeps its iteration there.
        let polls = ctx
            .nodes
            .get(&ctx.node.id)
            .and_then(|slot| slot.get(POLLS_KEY))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let max_polls = positive_u64(config, "max_polls", DEFAULT_MAX_POLLS);

        // Collect what has settled. Three sources, in order of authority:
        // a result already in hand, one delivered by a resume, then the runner.
        let delivered = delivered(&ctx);
        let mut results: Vec<(usize, Value)> = Vec::new();
        let mut failures: Vec<String> = Vec::new();
        for (index, item) in awaiting.iter().enumerate() {
            let state = if let Some(settled) = item.settled.clone() {
                Some(settled)
            } else if let Some(value) = item
                .ticket
                .as_ref()
                .and_then(|ticket| delivered.get(ticket.as_str()))
            {
                Some(TaskState::Done(value.clone()))
            } else if let Some(ticket) = item.ticket.as_ref() {
                match ctx.caps.tasks.as_ref() {
                    Some(runner) => Some(runner.poll(ticket).await?),
                    None => {
                        return Err(EngineError::Capability(format!(
                            "gate node {:?}: waiting on ticket {ticket:?} but no TaskRunner is \
                             injected; the spawn that made it must have used one",
                            ctx.node.id
                        )));
                    }
                }
            } else {
                None
            };
            match state {
                Some(TaskState::Done(value)) => results.push((index, value)),
                Some(TaskState::Failed(message)) => {
                    failures.push(message.clone());
                    results.push((index, json!({ "failed": true, "error": message })));
                }
                _ => {}
            }
        }

        let budget_spent = polls >= max_polls;
        let decision = policy.evaluate(results.len(), expected, budget_spent);

        let meta = json!({
            POLLS_KEY: polls + 1,
            "arrived": results.len(),
            "expected": expected,
            "failed": failures.len(),
        });

        match decision {
            Release::Wait => {
                let interval = positive_u64(config, "poll_interval_ms", DEFAULT_POLL_INTERVAL_MS);
                match WaitMode::from_config(config) {
                    // Suspending discards this activation's update, so the poll
                    // count deliberately does not advance: a suspended gate is
                    // not spending its poll budget, it is waiting for the host.
                    WaitMode::Suspend => Ok(NodeOutput::interrupt(
                        ctx.node.id.clone(),
                        json!({
                            "kind": "await",
                            "node": ctx.node.id,
                            "tickets": awaiting
                                .iter()
                                .filter_map(|a| a.ticket.clone())
                                .collect::<Vec<_>>(),
                            "arrived": results.len(),
                            "expected": expected,
                        }),
                    )),
                    WaitMode::Poll => Ok(NodeOutput::reenter_after(interval, meta)),
                }
            }
            Release::Timeout => match OnTimeout::from_config(config) {
                OnTimeout::Error => Err(EngineError::Capability(format!(
                    "gate node {:?}: timed out after {max_polls} polls with {} of {expected} \
                     results; raise `max_polls`/`poll_interval_ms`, relax `release`, or wire a \
                     `timeout` port",
                    ctx.node.id,
                    results.len()
                ))),
                OnTimeout::Route => {
                    Ok(NodeOutput::routed(emit_items(results), "timeout").with_meta(meta))
                }
                OnTimeout::Partial => Ok(NodeOutput::main(emit_items(results)).with_meta(meta)),
            },
            Release::Emit => Ok(NodeOutput::main(emit_items(results)).with_meta(meta)),
        }
    }
}

/// Turns collected results into output items, **ordered by ticket index**.
///
/// Completion order is not emission order. Two runs of the same graph can have
/// their tickets settle in different orders, and downstream must not be able to
/// tell — so results are sorted back into the order the tickets were listed,
/// and each carries its `paired_item` so the correlation survives.
fn emit_items(mut results: Vec<(usize, Value)>) -> Vec<Item> {
    results.sort_by_key(|(index, _)| *index);
    results
        .into_iter()
        .map(|(index, value)| Item::new(value).paired_with(index))
        .collect()
}

#[cfg(test)]
#[path = "gate_tests.rs"]
mod tests;
