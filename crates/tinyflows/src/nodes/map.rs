//! Bounded-concurrency fan-out: mapping a node's work over its input items.
//!
//! A node in [`ExecutionMode::PerItem`](super::ExecutionMode::PerItem) runs its
//! body once per input item. *How many* of those run at a time is the dial this
//! module owns:
//!
//! | `config.concurrency` | Behaviour |
//! |---|---|
//! | unset (or `1`) | strictly sequential — one item at a time, in input order |
//! | `n > 1` | at most `n` items in flight |
//! | `0` or `"all"` | every item in flight at once |
//!
//! Results are always returned in **input order** regardless of completion
//! order, and each output item carries `paired_item` so a downstream node can
//! correlate it back to the input that produced it. That is what makes a
//! fan-out node array-in/array-out: `split_out` → `agent(concurrency: 8)` →
//! `merge` behaves like a bounded `Promise.all`.
//!
//! ## Failure
//!
//! [`ItemErrorPolicy`] (`config.on_item_error`) decides what a failing item
//! does to the batch, and **its default follows the execution shape**:
//!
//! - **fanned out** (`concurrency` other than `1`) →
//!   [`Collect`](ItemErrorPolicy::Collect). One bad item must not discard the
//!   whole batch, so the failed slot is filled with an error item and the node
//!   still emits one output per input.
//! - **sequential** (`concurrency` unset or `1`) →
//!   [`FailFast`](ItemErrorPolicy::FailFast), exactly how per-item nodes
//!   behaved before fan-out existed, so the node's `on_error` / retry policy
//!   still sees the error.
//!
//! That split is deliberate. `tool_call`, `http_request`, and `memory` are
//! `per_item` *by default*, so collecting unconditionally would silently
//! disable `on_error`, retry, and the `error` port for the most ordinary nodes
//! in the engine — a graph that never asked for a fan-out would quietly stop
//! failing. Opting into concurrency is also opting into batch semantics; an
//! explicit `on_item_error` overrides the default in either direction.

use std::future::Future;

use futures_util::stream::StreamExt;
use serde_json::{Value, json};

use crate::data::Item;
use crate::error::Result;
use crate::expr::NullResolution;

/// The largest `concurrency` a node may request.
///
/// A graph is authored data — often by a model — so an absurd `concurrency`
/// (or a typo like `10000`) must not be able to open ten thousand simultaneous
/// agent turns against a host. Requests above this are clamped, not rejected,
/// so a workflow still runs; the clamp is `tracing::warn!`ed. Hosts layer their
/// own, usually lower, ceiling on top (e.g. a semaphore around agent runs).
pub(crate) const MAX_CONCURRENCY: usize = 64;

/// What a failing item does to the rest of the batch.
///
/// Read from `config.on_item_error`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ItemErrorPolicy {
    /// **The default when the node fans out** (`concurrency` other than `1`).
    /// The batch never fails: a failed item is replaced by an error item
    /// (`{ json: { error, failed: true }, … }`) in its own slot, so the node
    /// always emits exactly one item per input and a downstream node can branch
    /// on `=item.json.failed`.
    ///
    /// A fan-out is a batch of independent work — losing 19 good results
    /// because the 20th timed out is rarely what the author wanted, and with
    /// items completing concurrently there is no single "the error" to hand to
    /// `on_error` anyway.
    Collect,
    /// **The default when the node runs sequentially** (`concurrency` unset or
    /// `1`), which is how every per-item node behaved before fan-out existed.
    ///
    /// The first failure **in input order** fails the whole node, which then
    /// falls to the node's own `on_error` / retry policy. Remaining in-flight
    /// items are cancelled once no earlier item can still fail.
    ///
    /// This default is load-bearing, not merely conservative: `tool_call`,
    /// `http_request`, and `memory` are `per_item` *by default*, so collecting
    /// here would silently disable `on_error` / retry / the `error` port for
    /// the most ordinary nodes in the engine.
    FailFast,
    /// Failed items are dropped: the node emits only the successes, so the
    /// output array may be shorter than the input.
    Skip,
}

/// How a per-item node maps over its input: how many at a time, and what a
/// failure does.
#[derive(Debug, Clone, Copy)]
pub(crate) struct MapOptions {
    /// Maximum items in flight; `0` means unbounded.
    pub concurrency: usize,
    /// What a failing item does to the batch.
    pub on_item_error: ItemErrorPolicy,
}

impl Default for MapOptions {
    /// Sequential and fail-fast — exactly how per-item nodes behaved before
    /// fan-out existed.
    fn default() -> Self {
        Self {
            concurrency: 1,
            on_item_error: ItemErrorPolicy::FailFast,
        }
    }
}

/// Reads `concurrency` and `on_item_error` off a node's **raw** config.
///
/// Like [`execution_mode`](super::execution_mode), these select the execution
/// strategy itself rather than describing data, so they are read before (and
/// independently of) `=`-expression resolution — a concurrency bound that
/// depended on the current item would be meaningless, since the bound applies
/// to the batch.
///
/// Unrecognized values fall back to the defaults rather than erroring;
/// [`crate::validate`] rejects them at author time, where the message can point
/// at the offending node.
/// The run-level per-item concurrency ceiling, from
/// `trigger.config.max_item_concurrency`.
///
/// `None` when unset or not a positive integer, meaning each node is bounded
/// only by its own `concurrency` and [`MAX_CONCURRENCY`].
fn run_item_cap(run: &Value) -> Option<usize> {
    run.get("trigger")
        .and_then(|trigger| trigger.get("max_item_concurrency"))
        .and_then(Value::as_u64)
        .filter(|n| *n > 0)
        .map(|n| {
            usize::try_from(n)
                .unwrap_or(MAX_CONCURRENCY)
                .min(MAX_CONCURRENCY)
        })
}

#[must_use]
pub(crate) fn map_options(config: &Value, node_id: &str, run: &Value) -> MapOptions {
    let concurrency = match config.get("concurrency") {
        Some(Value::Number(n)) => n
            .as_u64()
            .map_or(1, |n| usize::try_from(n).unwrap_or(usize::MAX)),
        // `"all"` is the readable spelling of "no bound" — `Promise.all`.
        Some(Value::String(s)) if s == "all" => 0,
        _ => 1,
    };
    let concurrency = if concurrency > MAX_CONCURRENCY {
        tracing::warn!(
            node = %node_id,
            requested = concurrency,
            max = MAX_CONCURRENCY,
            "concurrency above the engine ceiling; clamping"
        );
        MAX_CONCURRENCY
    } else {
        concurrency
    };

    // A run-level ceiling the whole workflow shares, declared once on the
    // trigger instead of edited into every node. It only ever lowers a node's
    // own `concurrency`, so a node asking for less keeps its own number.
    //
    // `0` (the "all" spelling) means unbounded, which is exactly what a run-level
    // cap is for, so it is clamped like any other value rather than treated as
    // already-satisfied.
    let concurrency = match run_item_cap(run) {
        Some(cap) if concurrency == 0 || concurrency > cap => {
            tracing::debug!(
                node = %node_id,
                requested = concurrency,
                cap,
                "per-item concurrency lowered by the run-level cap"
            );
            cap
        }
        _ => concurrency,
    };

    // The default follows the execution shape: a fan-out collects (one bad item
    // must not discard the batch), while a sequential run keeps failing fast so
    // the node's `on_error` / retry policy still sees the error. An explicit
    // `on_item_error` overrides either way.
    let default_policy = if concurrency == 1 {
        ItemErrorPolicy::FailFast
    } else {
        ItemErrorPolicy::Collect
    };
    let on_item_error = match config.get("on_item_error").and_then(Value::as_str) {
        Some("fail_fast") => ItemErrorPolicy::FailFast,
        Some("skip") => ItemErrorPolicy::Skip,
        Some("collect") => ItemErrorPolicy::Collect,
        _ => default_policy,
    };

    MapOptions {
        concurrency,
        on_item_error,
    }
}

/// What one mapped item produced: its output item plus any null-resolution
/// diagnostics gathered while resolving that item's config.
pub(crate) type MappedItem = (Item, Vec<NullResolution>);

/// The error item substituted for a failed slot under
/// [`ItemErrorPolicy::Collect`].
///
/// Shaped as the standard capability
/// [envelope](crate::nodes::integration::envelope) so the accessors a graph
/// already uses keep working: `=item.json.failed` is the branch predicate and
/// `=item.json.error` the message, on every node kind that can fan out.
fn error_item(message: &str) -> Item {
    Item::new(crate::nodes::integration::envelope::from_parts(
        json!({ "error": message, "failed": true }),
        Some(message.to_string()),
        Value::Null,
    ))
}

/// Runs `f` over the `total` input indices with bounded concurrency, returning
/// the output items in **input order** with `paired_item` set, plus the union
/// of every item's diagnostics.
///
/// `f` receives an input **index** rather than the item itself: the caller
/// already holds the input slice (on `ctx.input`), and passing a borrowed item
/// across this generic boundary forces a higher-ranked bound that rustc cannot
/// satisfy for an `async` body that also borrows the node context. An index
/// keeps every lifetime concrete at the call site.
///
/// Items complete out of order; the results are re-sorted into input-order
/// slots before returning, so a fan-out never reorders a workflow's data.
///
/// # Errors
///
/// Only under [`ItemErrorPolicy::FailFast`], which returns the failure with the
/// **lowest input index** — not the first to complete, which would make the
/// error non-deterministic across runs. The other two policies never error.
pub(crate) async fn map_items<F, Fut>(
    total: usize,
    node_id: &str,
    observer: &dyn crate::observability::RunObserver,
    opts: MapOptions,
    f: F,
) -> Result<(Vec<Item>, Vec<NullResolution>)>
where
    F: Fn(usize) -> Fut,
    Fut: Future<Output = Result<MappedItem>>,
{
    // `buffer_unordered(0)` would never poll anything, so "unbounded" is spelled
    // as "as many as there are items".
    let in_flight = if opts.concurrency == 0 {
        total.max(1)
    } else {
        opts.concurrency
    };

    // Observer calls live inside the future so `buffer_unordered` only reports
    // work as started when a concurrency slot actually begins polling it.
    let mut stream = futures_util::stream::iter((0..total).map(|index| {
        let fut = f(index);
        async move {
            observer.on_item_start(node_id, index, total);
            let result = fut.await;
            observer.on_item_finish(node_id, index, total, result.is_ok());
            (index, result)
        }
    }))
    .buffer_unordered(in_flight);

    let mut slots: Vec<Option<std::result::Result<MappedItem, String>>> =
        (0..total).map(|_| None).collect();
    // Under `FailFast` the reported error must be the lowest-index one, but
    // items finish out of order. Track the lowest index seen so far; once every
    // *earlier* slot has also resolved, no smaller index can still fail, so that
    // error is final and dropping the stream cancels the rest.
    let mut fail_fast_error: Option<(usize, crate::error::EngineError)> = None;

    while let Some((index, result)) = stream.next().await {
        match result {
            Ok(mapped) => slots[index] = Some(Ok(mapped)),
            Err(err) => {
                if opts.on_item_error == ItemErrorPolicy::FailFast {
                    // Mark the slot resolved so the prefix check below can see it.
                    slots[index] = Some(Err(String::new()));
                    if fail_fast_error
                        .as_ref()
                        .is_none_or(|(seen, _)| index < *seen)
                    {
                        fail_fast_error = Some((index, err));
                    }
                    let lowest = fail_fast_error.as_ref().map_or(index, |(i, _)| *i);
                    if slots[..lowest].iter().all(Option::is_some) {
                        break; // dropping the stream cancels the remaining work
                    }
                    continue;
                }
                slots[index] = Some(Err(err.to_string()));
            }
        }
    }

    if let Some((_, err)) = fail_fast_error {
        return Err(err);
    }

    let mut items = Vec::with_capacity(total);
    let mut diagnostics = Vec::new();
    for (index, slot) in slots.into_iter().enumerate() {
        match slot {
            Some(Ok((item, diags))) => {
                items.push(item.paired_with(index));
                diagnostics.extend(diags);
            }
            Some(Err(message)) if opts.on_item_error == ItemErrorPolicy::Collect => {
                items.push(error_item(&message).paired_with(index));
            }
            // `Skip` drops the slot; `FailFast` already returned above. A `None`
            // slot is only reachable on the cancelled tail of a fail-fast break.
            _ => {}
        }
    }

    Ok((items, diagnostics))
}

#[cfg(test)]
#[path = "map_tests.rs"]
mod tests;
