//! The `gather` node: collect the lanes a [`scatter`] opened.
//!
//! # Not a topological barrier
//!
//! A `merge` waits for its declared predecessors — a static fact about the
//! graph. A gather cannot: how many lanes exist is decided at run time by the
//! scatter, from data. So its barrier is **data-driven**: it counts arrivals in
//! `nodes.<lane terminal>.lanes.*` against the `lane_count` the scatter
//! recorded, and asks to be re-run until its release policy is satisfied.
//!
//! That is also why it supports the same policies as a [`gate`]: once the wait
//! is a decision rather than a topological fact, "proceed on a quorum" and
//! "settle for what arrived" become expressible.
//!
//! [`scatter`]: super::scatter
//! [`gate`]: crate::nodes::integration::gate

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::data::Item;
use crate::error::{EngineError, Result};
use crate::nodes::release::{Release, ReleasePolicy};
use crate::nodes::{NodeContext, NodeExecutor, NodeOutput};

/// Default gap between checks, in milliseconds.
const DEFAULT_POLL_INTERVAL_MS: u64 = 5;

/// Default ceiling on checks before the wait is called spent.
const DEFAULT_MAX_POLLS: u64 = 500;

/// The slot key a gather records its poll count under.
const POLLS_KEY: &str = "polls";

/// What a gather does with a lane that failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OnLaneError {
    /// Emit the failure as an item, branchable with `=item.failed`.
    Collect,
    /// Drop it.
    Skip,
    /// Fail the whole gather.
    FailFast,
}

impl OnLaneError {
    fn from_config(config: &Value) -> Self {
        match config.get("on_lane_error").and_then(Value::as_str) {
            Some("skip") => Self::Skip,
            Some("fail_fast") => Self::FailFast,
            _ => Self::Collect,
        }
    }
}

/// Collects the lanes a `scatter` opened.
#[derive(Debug, Default, Clone)]
pub struct GatherNode;

/// One lane's recorded result.
struct Arrived {
    index: usize,
    items: Vec<Item>,
    failed: Option<String>,
}

/// Reads every lane slot recorded by this gather's lane-terminal predecessors.
///
/// Lanes are keyed by id under `nodes.<pred>.lanes`, which is what keeps N
/// concurrent activations of one node from clobbering each other — the reducer
/// merges objects key-by-key, so distinct lane keys never collide.
fn arrivals(ctx: &NodeContext<'_>, predecessors: &[String]) -> Vec<Arrived> {
    let mut arrived = Vec::new();
    for pred in predecessors {
        let Some(lanes) = ctx
            .nodes
            .get(pred)
            .and_then(|slot| slot.get("lanes"))
            .and_then(Value::as_object)
        else {
            continue;
        };
        for slot in lanes.values() {
            let index = slot
                .get("index")
                .and_then(Value::as_u64)
                .and_then(|n| usize::try_from(n).ok())
                .unwrap_or(0);
            let items: Vec<Item> = slot
                .get("items")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| serde_json::from_value::<Item>(item.clone()).ok())
                        .collect()
                })
                .unwrap_or_default();
            let failed = slot
                .get("status")
                .and_then(Value::as_str)
                .filter(|status| *status == "failed")
                .map(|_| {
                    slot.get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("lane failed")
                        .to_string()
                });
            arrived.push(Arrived {
                index,
                items,
                failed,
            });
        }
    }
    arrived
}

/// How many lanes the scatter upstream of this gather opened.
///
/// Read from the scatter's own slot rather than inferred from arrivals: a
/// gather that guessed "however many turned up" would release immediately on
/// the first one, which is the bug this exists to prevent.
fn expected_lanes(ctx: &NodeContext<'_>) -> Option<usize> {
    ctx.nodes.as_object()?.values().find_map(|slot| {
        slot.get("lane_count")
            .and_then(Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
    })
}

fn positive_u64(config: &Value, key: &str, default: u64) -> u64 {
    config
        .get(key)
        .and_then(Value::as_u64)
        .filter(|n| *n > 0)
        .unwrap_or(default)
}

#[async_trait]
impl NodeExecutor for GatherNode {
    async fn execute(&self, ctx: NodeContext<'_>) -> Result<NodeOutput> {
        let config = &ctx.node.config;
        let policy = ReleasePolicy::from_config(config, &ctx.node.id)?;

        // Predecessors are named in config rather than derived from edges: the
        // executor sees run state, not the graph. `validate` checks they match
        // the wiring.
        let predecessors: Vec<String> = config
            .get("from")
            .and_then(Value::as_array)
            .map(|from| {
                from.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();

        let arrived = arrivals(&ctx, &predecessors);
        let expected = expected_lanes(&ctx).unwrap_or(arrived.len());

        let polls = ctx
            .nodes
            .get(&ctx.node.id)
            .and_then(|slot| slot.get(POLLS_KEY))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let max_polls = positive_u64(config, "max_polls", DEFAULT_MAX_POLLS);
        let budget_spent = polls >= max_polls;

        let on_lane_error = OnLaneError::from_config(config);
        if on_lane_error == OnLaneError::FailFast
            && let Some(failure) = arrived.iter().find_map(|lane| lane.failed.as_ref())
        {
            return Err(EngineError::Capability(format!(
                "gather node {:?}: lane failed and `on_lane_error` is \"fail_fast\": {failure}",
                ctx.node.id
            )));
        }

        let meta = json!({
            POLLS_KEY: polls + 1,
            "lanes": expected,
            "arrived": arrived.len(),
            "failed": arrived.iter().filter(|lane| lane.failed.is_some()).count(),
        });

        match policy.evaluate(arrived.len(), expected, budget_spent) {
            Release::Wait => Ok(NodeOutput::reenter_after(
                positive_u64(config, "poll_interval_ms", DEFAULT_POLL_INTERVAL_MS),
                meta,
            )),
            Release::Timeout => Err(EngineError::Capability(format!(
                "gather node {:?}: only {} of {expected} lanes arrived within {max_polls} polls; \
                 raise `max_polls`, relax `release`, or use `release: \"timeout_partial\"`",
                ctx.node.id,
                arrived.len()
            ))),
            Release::Emit => {
                let partial = arrived.len() < expected;
                Ok(
                    NodeOutput::main(emit_items(arrived, on_lane_error)).with_meta(json!({
                        POLLS_KEY: polls + 1,
                        "lanes": expected,
                        "arrived": meta["arrived"],
                        "failed": meta["failed"],
                        "partial": partial,
                    })),
                )
            }
        }
    }
}

/// Flattens arrived lanes into output items, **ordered by lane index**.
///
/// Completion order is not emission order. Lanes finish in whatever order their
/// work takes, and two runs of the same graph can differ — so results are sorted
/// back into the order the scatter created them, and each item keeps its lane
/// index as `paired_item`. Without this a scatter/gather pair would be
/// nondeterministic in a way no downstream node could correct for.
fn emit_items(mut arrived: Vec<Arrived>, on_lane_error: OnLaneError) -> Vec<Item> {
    arrived.sort_by_key(|lane| lane.index);
    let mut out = Vec::new();
    for lane in arrived {
        match (&lane.failed, on_lane_error) {
            (Some(_), OnLaneError::Skip) => {}
            (Some(error), _) => out.push(
                Item::new(json!({ "failed": true, "error": error, "lane": lane.index }))
                    .paired_with(lane.index),
            ),
            (None, _) => out.extend(
                lane.items
                    .into_iter()
                    .map(|item| item.paired_with(lane.index)),
            ),
        }
    }
    out
}

#[cfg(test)]
#[path = "gather_tests.rs"]
mod tests;
