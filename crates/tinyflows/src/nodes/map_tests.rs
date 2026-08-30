use super::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

fn items(n: usize) -> Vec<Item> {
    (0..n).map(|i| Item::new(json!({ "i": i }))).collect()
}

fn opts(concurrency: usize, on_item_error: ItemErrorPolicy) -> MapOptions {
    MapOptions {
        concurrency,
        on_item_error,
    }
}

/// Tracks how many mapped bodies are in flight simultaneously, so a test can
/// assert the bound was actually honoured (and that a "parallel" mode really
/// did run things at once).
#[derive(Default)]
struct Gauge {
    live: AtomicUsize,
    peak: AtomicUsize,
}

impl Gauge {
    fn enter(&self) {
        let live = self.live.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(live, Ordering::SeqCst);
    }
    fn exit(&self) {
        self.live.fetch_sub(1, Ordering::SeqCst);
    }
    fn peak(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }
}

/// A mapped body that stays "in flight" long enough for its peers to start,
/// so the gauge can observe real overlap rather than a lucky interleaving.
async fn tick() {
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
async fn results_keep_input_order_even_when_completion_order_is_reversed() {
    let input = items(5);
    let input = &input;
    // Later items finish first: item 0 yields the most, item 4 the least.
    let (out, _) = map_items(
        input.len(),
        "n",
        &crate::observability::NoopObserver,
        opts(0, ItemErrorPolicy::Collect),
        |index| async move {
            for _ in 0..(5 - index) * 4 {
                tokio::task::yield_now().await;
            }
            Ok((Item::new(input[index].json.clone()), vec![]))
        },
    )
    .await
    .expect("map");

    assert_eq!(out.len(), 5);
    for (index, item) in out.iter().enumerate() {
        assert_eq!(item.json["i"], index, "output must be in input order");
        assert_eq!(item.paired_item, Some(index), "pairing tracks the input");
    }
}

#[tokio::test]
async fn concurrency_one_is_strictly_sequential() {
    let input = items(6);
    let gauge = Arc::new(Gauge::default());
    let g = gauge.clone();
    let input = &input;
    let (out, _) = map_items(
        input.len(),
        "n",
        &crate::observability::NoopObserver,
        opts(1, ItemErrorPolicy::Collect),
        move |index| {
            let g = g.clone();
            async move {
                g.enter();
                tick().await;
                g.exit();
                Ok((Item::new(input[index].json.clone()), vec![]))
            }
        },
    )
    .await
    .expect("map");

    assert_eq!(out.len(), 6);
    assert_eq!(gauge.peak(), 1, "unset/1 concurrency must not overlap work");
}

#[tokio::test]
async fn bounded_concurrency_overlaps_but_respects_the_ceiling() {
    let input = items(12);
    let gauge = Arc::new(Gauge::default());
    let g = gauge.clone();
    let input = &input;
    let (out, _) = map_items(
        input.len(),
        "n",
        &crate::observability::NoopObserver,
        opts(4, ItemErrorPolicy::Collect),
        move |index| {
            let g = g.clone();
            async move {
                g.enter();
                tick().await;
                g.exit();
                Ok((Item::new(input[index].json.clone()), vec![]))
            }
        },
    )
    .await
    .expect("map");

    assert_eq!(out.len(), 12);
    assert!(gauge.peak() > 1, "bounded fan-out must actually overlap");
    assert!(
        gauge.peak() <= 4,
        "never more than `concurrency` in flight, saw {}",
        gauge.peak()
    );
}

#[tokio::test]
async fn zero_concurrency_runs_every_item_at_once() {
    let input = items(7);
    let gauge = Arc::new(Gauge::default());
    let g = gauge.clone();
    let input = &input;
    let (out, _) = map_items(
        input.len(),
        "n",
        &crate::observability::NoopObserver,
        opts(0, ItemErrorPolicy::Collect),
        move |index| {
            let g = g.clone();
            async move {
                g.enter();
                tick().await;
                g.exit();
                Ok((Item::new(input[index].json.clone()), vec![]))
            }
        },
    )
    .await
    .expect("map");

    assert_eq!(out.len(), 7);
    assert_eq!(gauge.peak(), 7, "`0`/`\"all\"` means unbounded");
}

#[tokio::test]
async fn collect_substitutes_an_error_item_and_keeps_the_array_length() {
    let input = items(4);
    let input = &input;
    let (out, _) = map_items(
        input.len(),
        "n",
        &crate::observability::NoopObserver,
        opts(0, ItemErrorPolicy::Collect),
        |index| async move {
            if index == 2 {
                return Err(crate::error::EngineError::Capability("boom".into()));
            }
            Ok((Item::new(input[index].json.clone()), vec![]))
        },
    )
    .await
    .expect("collect never fails the batch");

    assert_eq!(out.len(), 4, "one output per input");
    assert_eq!(out[2].json["json"]["failed"], true);
    assert!(
        out[2].json["json"]["error"]
            .as_str()
            .expect("error message")
            .contains("boom")
    );
    assert_eq!(out[2].paired_item, Some(2));
    // Its neighbours are untouched successes.
    assert_eq!(out[1].json["i"], 1);
    assert_eq!(out[3].json["i"], 3);
}

#[tokio::test]
async fn skip_drops_failures_and_shortens_the_array() {
    let input = items(4);
    let input = &input;
    let (out, _) = map_items(
        input.len(),
        "n",
        &crate::observability::NoopObserver,
        opts(0, ItemErrorPolicy::Skip),
        |index| async move {
            if index % 2 == 0 {
                return Err(crate::error::EngineError::Capability("nope".into()));
            }
            Ok((Item::new(input[index].json.clone()), vec![]))
        },
    )
    .await
    .expect("skip never fails the batch");

    assert_eq!(out.len(), 2);
    assert_eq!(out[0].json["i"], 1);
    assert_eq!(out[1].json["i"], 3);
    // Pairing still points at the original input index, not the compacted one.
    assert_eq!(out[0].paired_item, Some(1));
    assert_eq!(out[1].paired_item, Some(3));
}

#[tokio::test]
async fn fail_fast_reports_the_lowest_index_error_not_the_first_to_finish() {
    let input = items(6);
    // Item 4 fails immediately; item 1 fails only after yielding. Input order
    // must win, so the reported error is item 1's.
    let err = map_items(
        input.len(),
        "n",
        &crate::observability::NoopObserver,
        opts(0, ItemErrorPolicy::FailFast),
        |index| async move {
            if index == 4 {
                return Err(crate::error::EngineError::Capability("late-index".into()));
            }
            if index == 1 {
                tick().await;
                return Err(crate::error::EngineError::Capability("early-index".into()));
            }
            Ok((Item::new(json!({ "i": index })), vec![]))
        },
    )
    .await
    .expect_err("fail_fast must surface an error");

    assert!(
        err.to_string().contains("early-index"),
        "expected the lowest-index failure, got {err}"
    );
}

#[tokio::test]
async fn empty_input_yields_no_items_and_no_error() {
    let (out, diags) = map_items(
        0,
        "n",
        &crate::observability::NoopObserver,
        opts(0, ItemErrorPolicy::Collect),
        |_| async { unreachable!("no items to map") },
    )
    .await
    .expect("map");
    assert!(out.is_empty());
    assert!(diags.is_empty());
}

#[tokio::test]
async fn diagnostics_from_every_item_are_unioned() {
    let input = items(3);
    let (_, diags) = map_items(
        input.len(),
        "n",
        &crate::observability::NoopObserver,
        opts(0, ItemErrorPolicy::Collect),
        |index| async move {
            Ok((
                Item::new(Value::Null),
                vec![NullResolution {
                    location: format!("config.prompt[{index}]"),
                    expression: "=item.missing".to_string(),
                }],
            ))
        },
    )
    .await
    .expect("map");
    assert_eq!(diags.len(), 3);
}

// --- map_options ---

#[test]
fn options_default_to_sequential_and_fail_fast() {
    // The pre-fan-out behaviour, unchanged: one at a time, and a failure
    // reaches the node's own `on_error` / retry policy.
    let o = map_options(&json!({}), "n", &Value::Null);
    assert_eq!(o.concurrency, 1, "unset concurrency stays sequential");
    assert_eq!(o.on_item_error, ItemErrorPolicy::FailFast);
}

#[test]
fn fanning_out_flips_the_default_policy_to_collect() {
    // Opting into concurrency opts into batch semantics: one bad item must
    // not discard the other results.
    for concurrency in [0, 2, 8] {
        let o = map_options(&json!({ "concurrency": concurrency }), "n", &Value::Null);
        assert_eq!(
            o.on_item_error,
            ItemErrorPolicy::Collect,
            "concurrency {concurrency} should default to collect"
        );
    }
    // ...but an explicit `concurrency: 1` is not a fan-out.
    assert_eq!(
        map_options(&json!({ "concurrency": 1 }), "n", &Value::Null).on_item_error,
        ItemErrorPolicy::FailFast
    );
}

#[test]
fn an_explicit_policy_overrides_the_shape_derived_default() {
    assert_eq!(
        map_options(&json!({ "on_item_error": "collect" }), "n", &Value::Null).on_item_error,
        ItemErrorPolicy::Collect,
        "sequential can opt into collecting"
    );
    assert_eq!(
        map_options(
            &json!({ "concurrency": 8, "on_item_error": "fail_fast" }),
            "n",
            &Value::Null
        )
        .on_item_error,
        ItemErrorPolicy::FailFast,
        "a fan-out can opt back into failing fast"
    );
}

#[test]
fn options_read_numeric_and_all_concurrency() {
    assert_eq!(
        map_options(&json!({ "concurrency": 8 }), "n", &Value::Null).concurrency,
        8
    );
    assert_eq!(
        map_options(&json!({ "concurrency": 0 }), "n", &Value::Null).concurrency,
        0
    );
    assert_eq!(
        map_options(&json!({ "concurrency": "all" }), "n", &Value::Null).concurrency,
        0,
        "`\"all\"` is the readable spelling of unbounded"
    );
}

/// The run-level cap lowers a node that asked for more, and leaves alone a
/// node that asked for less — it is a ceiling, not an assignment.
#[test]
fn the_run_level_cap_only_lowers_a_nodes_own_concurrency() {
    let run = json!({ "trigger": { "max_item_concurrency": 4 } });
    assert_eq!(
        map_options(&json!({ "concurrency": 16 }), "n", &run).concurrency,
        4,
        "a node above the run cap is lowered to it"
    );
    assert_eq!(
        map_options(&json!({ "concurrency": 2 }), "n", &run).concurrency,
        2,
        "a node below the run cap keeps its own smaller value"
    );
}

/// `"all"` / `0` means unbounded, which is exactly the case a run-level cap
/// exists to bound — so it is capped rather than treated as satisfied.
#[test]
fn the_run_level_cap_bounds_an_unbounded_node() {
    let run = json!({ "trigger": { "max_item_concurrency": 3 } });
    assert_eq!(
        map_options(&json!({ "concurrency": "all" }), "n", &run).concurrency,
        3
    );
    assert_eq!(
        map_options(&json!({ "concurrency": 0 }), "n", &run).concurrency,
        3
    );
}

/// An absent, zero, or malformed cap leaves node concurrency untouched.
#[test]
fn a_missing_or_invalid_run_level_cap_changes_nothing() {
    for run in [
        json!({}),
        json!({ "trigger": {} }),
        json!({ "trigger": { "max_item_concurrency": 0 } }),
        json!({ "trigger": { "max_item_concurrency": "lots" } }),
    ] {
        assert_eq!(
            map_options(&json!({ "concurrency": 8 }), "n", &run).concurrency,
            8,
            "cap {run} should not change the node's own concurrency"
        );
    }
}

#[test]
fn options_clamp_an_absurd_concurrency_instead_of_failing_the_run() {
    let o = map_options(&json!({ "concurrency": 10_000 }), "n", &Value::Null);
    assert_eq!(o.concurrency, MAX_CONCURRENCY);
}

#[test]
fn options_ignore_a_nonsense_concurrency_and_stay_sequential() {
    // `validate` rejects these at author time; at run time they must not
    // silently become unbounded.
    assert_eq!(
        map_options(&json!({ "concurrency": "lots" }), "n", &Value::Null).concurrency,
        1
    );
    assert_eq!(
        map_options(&json!({ "concurrency": -3 }), "n", &Value::Null).concurrency,
        1
    );
    assert_eq!(
        map_options(&json!({ "concurrency": true }), "n", &Value::Null).concurrency,
        1
    );
}

#[test]
fn options_read_every_item_error_policy() {
    let policy = |v| {
        map_options(
            &json!({ "concurrency": 4, "on_item_error": v }),
            "n",
            &Value::Null,
        )
        .on_item_error
    };
    assert_eq!(policy("fail_fast"), ItemErrorPolicy::FailFast);
    assert_eq!(policy("skip"), ItemErrorPolicy::Skip);
    assert_eq!(policy("collect"), ItemErrorPolicy::Collect);
    assert_eq!(
        policy("bogus"),
        ItemErrorPolicy::Collect,
        "unknown policies fall back to the shape-derived default"
    );
}
