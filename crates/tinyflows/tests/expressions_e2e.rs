#![cfg(feature = "mock")]
//! End-to-end tests for jaq/jq expression evaluation inside running workflows.
//!
//! Config strings prefixed with `=` are expressions (see `src/expr.rs`): a simple
//! dotted path resolves by segment-walk, anything else runs as a jq program over
//! the evaluation scope `{ item, run }`. These tests exercise expressions in the
//! nodes that consume them — `transform` (its `set` map), `switch` (its case key),
//! and a `condition` fed by a computed field — plus a `split_out` → `transform`
//! combo, and assert the *computed values* land in the run output.
//!
//! Gated behind the `mock` feature, so plain `cargo test` skips it while
//! `cargo test --all-features` runs it.

use serde_json::{Value, json};
use tinyflows::caps::mock::mock_capabilities;
use tinyflows::compiler::compile;
use tinyflows::engine::run;
use tinyflows::model::{Edge, Node, NodeKind, TriggerKind, WorkflowGraph};

/// Builds a node with the given id, kind, and config.
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

/// Builds a trigger node with the given firing mode.
fn trigger(id: &str, kind: TriggerKind) -> Node {
    node(id, NodeKind::Trigger, json!({ "kind": kind }))
}

/// Builds an edge from `from_node`'s `from_port` into `to_node`'s `main` port.
fn edge(from_node: &str, from_port: &str, to_node: &str) -> Edge {
    Edge {
        from_node: from_node.to_string(),
        from_port: from_port.to_string(),
        to_node: to_node.to_string(),
        to_port: "main".to_string(),
    }
}

/// A `transform` whose `set` map uses jq programs — `add`, `length`, `map`, and
/// array indexing — over the incoming item. Each computed field must appear on
/// the emitted item with the right value.
#[tokio::test]
async fn transform_evaluates_jq_aggregations() {
    let graph = WorkflowGraph {
        name: "jq_transform".to_string(),
        nodes: vec![
            trigger("start", TriggerKind::Manual),
            node(
                "compute",
                NodeKind::Transform,
                json!({
                    "set": {
                        "total": "=.item.nums | add",
                        "count": "=.item.nums | length",
                        "doubled": "=.item.nums | map(. * 2)",
                        "first": "=.item.nums[0]"
                    }
                }),
            ),
        ],
        edges: vec![edge("start", "main", "compute")],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let outcome = run(
        &compiled,
        json!({ "nums": [1, 2, 3, 4] }),
        &mock_capabilities(),
    )
    .await
    .expect("run");

    let item = &outcome.output["nodes"]["compute"]["items"][0]["json"];
    assert_eq!(item["total"], json!(10), "add should sum the array");
    assert_eq!(item["count"], json!(4), "length should count the elements");
    assert_eq!(
        item["doubled"],
        json!([2, 4, 6, 8]),
        "map(. * 2) should double each element"
    );
    assert_eq!(item["first"], json!(1), "index [0] should take the first");
    // The source field is preserved alongside the computed ones.
    assert_eq!(item["nums"], json!([1, 2, 3, 4]));
}

/// A `switch` whose case key is a jq program computing a discriminant string.
/// Only the branch matching the computed port must run.
#[tokio::test]
async fn switch_routes_on_jq_discriminant() {
    let graph = WorkflowGraph {
        name: "jq_switch".to_string(),
        nodes: vec![
            trigger("start", TriggerKind::Manual),
            // priority > 5 => "high", otherwise "low" — a jq-computed case key.
            node(
                "route",
                NodeKind::Switch,
                json!({ "expression": "=if .item.priority > 5 then \"high\" else \"low\" end" }),
            ),
            node("high", NodeKind::OutputParser, Value::Null),
            node("low", NodeKind::OutputParser, Value::Null),
        ],
        edges: vec![
            edge("start", "main", "route"),
            edge("route", "high", "high"),
            edge("route", "low", "low"),
        ],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let outcome = run(&compiled, json!({ "priority": 9 }), &mock_capabilities())
        .await
        .expect("run");

    assert!(
        !outcome.output["nodes"]["high"]["items"].is_null(),
        "priority 9 should route to the `high` branch"
    );
    assert!(
        outcome.output["nodes"]["low"].is_null(),
        "the `low` branch should not have run"
    );
}

/// A `switch` keyed by a plain `field` discriminant (no expression): the field's
/// value on the first input item selects the port.
#[tokio::test]
async fn switch_routes_on_field_discriminant() {
    let graph = WorkflowGraph {
        name: "field_switch".to_string(),
        nodes: vec![
            trigger("start", TriggerKind::Manual),
            node("route", NodeKind::Switch, json!({ "field": "kind" })),
            node("alpha", NodeKind::OutputParser, Value::Null),
            node("beta", NodeKind::OutputParser, Value::Null),
        ],
        edges: vec![
            edge("start", "main", "route"),
            edge("route", "alpha", "alpha"),
            edge("route", "beta", "beta"),
        ],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let outcome = run(&compiled, json!({ "kind": "beta" }), &mock_capabilities())
        .await
        .expect("run");

    assert!(
        !outcome.output["nodes"]["beta"]["items"].is_null(),
        "kind `beta` should route to the `beta` branch"
    );
    assert!(
        outcome.output["nodes"]["alpha"].is_null(),
        "the `alpha` branch should not have run"
    );
}

/// A `condition` driven by a field that a preceding `transform` computed with a
/// jq comparison. The boolean result selects the `true`/`false` port.
#[tokio::test]
async fn condition_driven_by_computed_field() {
    let graph = WorkflowGraph {
        name: "computed_condition".to_string(),
        nodes: vec![
            trigger("start", TriggerKind::Manual),
            // Compute a boolean `passed` field via a jq comparison expression.
            node(
                "grade",
                NodeKind::Transform,
                json!({ "set": { "passed": "=.item.score >= 50" } }),
            ),
            node("check", NodeKind::Condition, json!({ "field": "passed" })),
            node("accepted", NodeKind::OutputParser, Value::Null),
            node("rejected", NodeKind::OutputParser, Value::Null),
        ],
        edges: vec![
            edge("start", "main", "grade"),
            edge("grade", "main", "check"),
            edge("check", "true", "accepted"),
            edge("check", "false", "rejected"),
        ],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let outcome = run(&compiled, json!({ "score": 73 }), &mock_capabilities())
        .await
        .expect("run");

    // The transform actually computed the boolean.
    assert_eq!(
        outcome.output["nodes"]["grade"]["items"][0]["json"]["passed"],
        json!(true),
        "score 73 >= 50 should compute `passed = true`"
    );
    assert!(
        !outcome.output["nodes"]["accepted"]["items"].is_null(),
        "a passing score should route to the `true` branch"
    );
    assert!(
        outcome.output["nodes"]["rejected"].is_null(),
        "the `false` branch should not have run"
    );
}

/// A `split_out` → `transform` combo: fan an array into one item per element,
/// then compute a field per item with a jq expression. Every emitted item must
/// carry the per-element computed value.
#[tokio::test]
async fn split_out_then_transform_computes_per_item() {
    let graph = WorkflowGraph {
        name: "split_transform".to_string(),
        nodes: vec![
            trigger("start", TriggerKind::Manual),
            node("fan", NodeKind::SplitOut, json!({ "path": "orders" })),
            // Each fanned item is one order object; compute a `total` per item.
            node(
                "price",
                NodeKind::Transform,
                json!({ "set": { "total": "=.item.qty * .item.unit" } }),
            ),
        ],
        edges: vec![edge("start", "main", "fan"), edge("fan", "main", "price")],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let outcome = run(
        &compiled,
        json!({
            "orders": [
                { "qty": 2, "unit": 5 },
                { "qty": 3, "unit": 10 }
            ]
        }),
        &mock_capabilities(),
    )
    .await
    .expect("run");

    // split_out fanned the two orders into two items.
    let items = outcome.output["nodes"]["price"]["items"]
        .as_array()
        .expect("transform produced an items array");
    assert_eq!(items.len(), 2, "two orders should fan out into two items");
    assert_eq!(
        items[0]["json"]["total"],
        json!(10),
        "first order total should be 2 * 5"
    );
    assert_eq!(
        items[1]["json"]["total"],
        json!(30),
        "second order total should be 3 * 10"
    );
}
