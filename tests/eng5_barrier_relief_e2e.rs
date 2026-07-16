#![cfg(feature = "mock")]
//! Eng §5 regression: the mixed fan-in barrier deadlock and its
//! `BarrierRelief`-based fix.
//!
//! A fan-in (`merge`) node with more than one predecessor is lowered as an
//! all-waiting barrier (see `src/engine.rs`'s edge-lowering loop) so a
//! downstream merge never fires off a stale superstep snapshot before a
//! *taken* conditional branch's items have committed (the data-loss failure
//! mode a plain edge would reintroduce). But when one predecessor is only
//! reachable via a conditional branch that this run does *not* take, that
//! predecessor never arrives on its own — without a relief, the barrier
//! would wait for it forever.
//!
//! These tests build the mixed fan-in repro graph directly (one
//! unconditionally-reachable predecessor `c`, one conditional-only
//! predecessor `a`) and assert both directions: the untaken-branch case must
//! not deadlock (guarded by a `tokio::time::timeout` so a regression fails
//! fast instead of hanging the suite), and the taken-branch case must not
//! lose `a`'s item. Two more tests pin the pure-conditional-join and
//! pure-unconditional-fan-in cases the old `is_conditional_join` special-cased,
//! now handled uniformly by `conditional_predecessors` + `BarrierRelief`.
//!
//! Gated behind the `mock` cargo feature, so plain `cargo test` skips it while
//! `cargo test --all-features` runs it (mirrors `branching_e2e.rs` /
//! `hardening_branching_e2e.rs`).

use std::collections::HashSet;
use std::time::Duration;

use serde_json::{Value, json};
use tinyflows::caps::mock::mock_capabilities;
use tinyflows::compiler::compile;
use tinyflows::engine::run;
use tinyflows::model::{Edge, Node, NodeKind, TriggerKind, WorkflowGraph};

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

fn trigger(id: &str) -> Node {
    node(
        id,
        NodeKind::Trigger,
        json!({ "kind": TriggerKind::Manual }),
    )
}

/// Edge from `from_node`'s `from_port` into `to_node`'s `"main"` port.
fn edge(from_node: &str, from_port: &str, to_node: &str) -> Edge {
    Edge {
        from_node: from_node.to_string(),
        from_port: from_port.to_string(),
        to_node: to_node.to_string(),
        to_port: "main".to_string(),
    }
}

/// A `transform` node that passes its input through, stamped with `tag` so a
/// downstream `merge`'s items can be traced back to which node produced them.
fn tagged(id: &str, tag: &str) -> Node {
    node(id, NodeKind::Transform, json!({ "set": { "tag": tag } }))
}

/// The Eng §5 repro graph: `start` fans out to `cond` and `c` (both
/// unconditionally reachable — a parallel fan-out, not a branch). `cond`
/// routes on `flag`: `true -> a`, `false -> b`. `a` and `c` both feed the
/// merge `m`; `b` is a dead end (never wired into `m`).
///
/// `c` is `m`'s one unconditionally-reachable predecessor (reachable from
/// `start` via a plain `"main"`-port fan-out); `a` is reachable only through
/// `cond`'s `true` port, so it is `m`'s one conditional predecessor.
fn mixed_fan_in_graph() -> WorkflowGraph {
    WorkflowGraph {
        name: "eng5_mixed_fan_in".to_string(),
        nodes: vec![
            trigger("start"),
            node("cond", NodeKind::Condition, json!({ "field": "flag" })),
            tagged("a", "a"),
            tagged("b", "b"),
            tagged("c", "c"),
            node("m", NodeKind::Merge, Value::Null),
        ],
        edges: vec![
            edge("start", "main", "cond"),
            edge("start", "main", "c"),
            edge("cond", "true", "a"),
            edge("cond", "false", "b"),
            edge("a", "main", "m"),
            edge("c", "main", "m"),
        ],
        ..Default::default()
    }
}

fn merge_tags(output: &Value, node_id: &str) -> HashSet<String> {
    output["nodes"][node_id]["items"]
        .as_array()
        .unwrap_or_else(|| panic!("{node_id} emitted no items array: {output}"))
        .iter()
        .map(|item| {
            item["json"]["tag"]
                .as_str()
                .unwrap_or_else(|| panic!("merged item missing `tag`: {item}"))
                .to_string()
        })
        .collect()
}

/// flag:false — `cond` takes the `false` branch (to `b`, a dead end never
/// wired into `m`), so `a` never runs. Pre-fix, `m`'s all-waiting barrier
/// (required = {a, c}) held forever on `a`; the `BarrierRelief` registered
/// for (`cond`, `a`, `m`) fires a phantom arrival for `a` when `cond`
/// completes without routing to it, so `m` fires off `c`'s real arrival
/// instead of deadlocking.
#[tokio::test]
async fn mixed_fan_in_conditional_not_taken_completes() {
    let compiled = compile(&mixed_fan_in_graph()).expect("compile");
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        run(&compiled, json!({ "flag": false }), &mock_capabilities()),
    )
    .await
    .expect("mixed fan-in must not deadlock when the conditional branch is untaken")
    .expect("run");
    let out = &outcome.output;

    assert!(
        out["nodes"]["a"].is_null(),
        "the untaken `true` branch (`a`) must not have run"
    );
    let tags = merge_tags(out, "m");
    assert_eq!(
        tags,
        HashSet::from(["c".to_string()]),
        "m should fire with only c's item once its barrier is relieved of a"
    );
}

/// flag:true — `cond` takes the `true` branch, so `a` DOES run this time.
/// This is the data-loss guard: the fix must not weaken `m`'s barrier into a
/// plain edge (which would let `m` fire off the superstep snapshot *before*
/// `a`'s item commits, silently dropping it) — `m` must still wait for BOTH
/// `a` and `c` and include both items.
#[tokio::test]
async fn mixed_fan_in_conditional_taken_includes_both_items() {
    let compiled = compile(&mixed_fan_in_graph()).expect("compile");
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        run(&compiled, json!({ "flag": true }), &mock_capabilities()),
    )
    .await
    .expect("mixed fan-in must not deadlock when the conditional branch is taken")
    .expect("run");
    let out = &outcome.output;

    assert!(
        out["nodes"]["b"].is_null(),
        "the untaken `false` branch (`b`) must not have run"
    );
    let tags = merge_tags(out, "m");
    assert_eq!(
        tags,
        HashSet::from(["a".to_string(), "c".to_string()]),
        "no data loss: m must include a's item as well as c's"
    );
}

/// Pure conditional join: both of `m`'s predecessors sit behind `cond`'s two
/// ports (no unconditionally-reachable predecessor at all). Only the taken
/// branch ever runs, so `m` must fire with exactly that branch's item — the
/// classic conditional-join case the old (now-removed) `is_conditional_join`
/// special-cased by lowering the edges as plain instead of waiting. The
/// uniform all-waiting-plus-relief design must reproduce the same observable
/// behavior: relief registrations for *both* `a` and `b` let whichever
/// branch's own real arrival satisfy the barrier once the other's relief has
/// already landed.
fn pure_conditional_join_graph() -> WorkflowGraph {
    WorkflowGraph {
        name: "eng5_pure_conditional_join".to_string(),
        nodes: vec![
            trigger("start"),
            node("cond", NodeKind::Condition, json!({ "field": "flag" })),
            tagged("a", "a"),
            tagged("b", "b"),
            node("m", NodeKind::Merge, Value::Null),
        ],
        edges: vec![
            edge("start", "main", "cond"),
            edge("cond", "true", "a"),
            edge("cond", "false", "b"),
            edge("a", "main", "m"),
            edge("b", "main", "m"),
        ],
        ..Default::default()
    }
}

#[tokio::test]
async fn pure_conditional_join_regression() {
    let compiled = compile(&pure_conditional_join_graph()).expect("compile");

    for (flag, taken, untaken) in [(true, "a", "b"), (false, "b", "a")] {
        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            run(&compiled, json!({ "flag": flag }), &mock_capabilities()),
        )
        .await
        .unwrap_or_else(|_| panic!("pure conditional join must not deadlock for flag:{flag}"))
        .expect("run");
        let out = &outcome.output;

        assert!(
            out["nodes"][untaken].is_null(),
            "the untaken branch `{untaken}` must not have run for flag:{flag}"
        );
        let tags = merge_tags(out, "m");
        assert_eq!(
            tags,
            HashSet::from([taken.to_string()]),
            "m should fire with exactly the taken branch's item for flag:{flag}"
        );
    }
}

/// Pure unconditional fan-in: `start` fans out to `a` and `b` in parallel (no
/// branching involved) and both always run; `m` waits for both, exactly as
/// before — `conditional_predecessors` is empty for this graph, so no
/// `BarrierRelief` is registered and the barrier behaves exactly as it did
/// pre-fix.
#[tokio::test]
async fn pure_unconditional_fan_in_regression() {
    let graph = WorkflowGraph {
        name: "eng5_pure_unconditional_fan_in".to_string(),
        nodes: vec![
            trigger("start"),
            tagged("a", "a"),
            tagged("b", "b"),
            node("m", NodeKind::Merge, Value::Null),
        ],
        edges: vec![
            edge("start", "main", "a"),
            edge("start", "main", "b"),
            edge("a", "main", "m"),
            edge("b", "main", "m"),
        ],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        run(&compiled, json!({ "seed": 1 }), &mock_capabilities()),
    )
    .await
    .expect("pure unconditional fan-in must not deadlock")
    .expect("run");
    let out = &outcome.output;

    let tags = merge_tags(out, "m");
    assert_eq!(
        tags,
        HashSet::from(["a".to_string(), "b".to_string()]),
        "m must include both a's and b's items"
    );
}
