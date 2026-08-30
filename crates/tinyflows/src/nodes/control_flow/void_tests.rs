use super::*;
use crate::caps::Capabilities;
use crate::caps::mock::mock_capabilities;
use crate::data::Item;
use crate::model::{Node, NodeKind};
use serde_json::{Value, json};

fn void_node(id: &str, config: Value) -> Node {
    Node {
        id: id.to_string(),
        kind: NodeKind::Void,
        type_version: 1,
        name: id.to_string(),
        config,
        ports: Vec::new(),
        position: None,
    }
}

async fn run_void(caps: &Capabilities, node: &Node, input: &[Item]) -> NodeOutput {
    let run = Value::Null;
    let ctx = NodeContext {
        node,
        input,
        run: &run,
        nodes: &Value::Null,
        caps,
        agents: &[],
        observer: &crate::observability::NoopObserver,
        token: crate::engine::CancellationToken::new(),
        lane: None,
        resume: None,
        step: 0,
    };
    VoidNode.execute(ctx).await.expect("execute")
}

#[tokio::test]
async fn discards_every_input_item_and_emits_nothing() {
    // Test 1 (spec): the branch ends here — no items, no port to route on, no
    // control instruction that could put the node back on the active set.
    let caps = mock_capabilities();
    let node = void_node("sink", Value::Null);
    let input = vec![
        Item::new(json!({ "id": "a" })),
        Item::new(json!({ "id": "b" })),
        Item::new(json!({ "id": "c" })),
    ];

    let out = run_void(&caps, &node, &input).await;

    assert!(out.items.is_empty(), "void must emit no items");
    assert!(out.port.is_none(), "void must not route on a port");
    assert!(out.control.is_none(), "void must not re-enter or interrupt");
}

#[tokio::test]
async fn records_the_discarded_count_in_meta() {
    // Test 2 (spec): the count is the node's only trace, so it must be exact.
    let caps = mock_capabilities();
    let node = void_node("sink", Value::Null);
    let input = vec![
        Item::new(json!({ "id": "a" })),
        Item::new(json!({ "id": "b" })),
        Item::new(json!({ "id": "c" })),
    ];

    let out = run_void(&caps, &node, &input).await;

    assert_eq!(out.meta, Some(json!({ "discarded": 3 })));
}

#[tokio::test]
async fn empty_input_still_reports_zero_discarded() {
    // Test 3 (spec): "activated with nothing to drop" must stay distinguishable
    // from "never activated", which shows up as an absent slot rather than a
    // zero. Without the meta both look identical downstream.
    let caps = mock_capabilities();
    let node = void_node("sink", Value::Null);

    let out = run_void(&caps, &node, &[]).await;

    assert_eq!(out.meta, Some(json!({ "discarded": 0 })));
    assert!(out.items.is_empty());
}

#[tokio::test]
async fn ignores_config_entirely_and_emits_no_diagnostics() {
    // Test 4 (spec): the kind declares no config fields, and in particular it
    // must not resolve `=` expressions — a void that emitted null-binding
    // diagnostics would be noise about data it exists to throw away.
    let caps = mock_capabilities();
    let node = void_node(
        "sink",
        json!({
            "reason": "fire and forget",
            "key": "=item.does.not.exist",
            "on_error": "stop",
        }),
    );
    let input = vec![Item::new(json!({ "id": "a" }))];

    let out = run_void(&caps, &node, &input).await;

    assert!(out.items.is_empty());
    assert!(
        out.diagnostics.is_empty(),
        "void resolves no expressions, so it can raise no null-binding diagnostics"
    );
    assert_eq!(out.meta, Some(json!({ "discarded": 1 })));
}

#[tokio::test]
async fn is_pure_across_repeated_activations() {
    // Test 5 (spec): the node touches no capability and holds no state, so a
    // second activation behaves exactly like the first. This is what lets a
    // void sit inside a loop body without accumulating anything.
    let caps = mock_capabilities();
    let node = void_node("sink", Value::Null);
    let input = vec![Item::new(json!({ "id": "a" }))];

    let first = run_void(&caps, &node, &input).await;
    let second = run_void(&caps, &node, &input).await;

    assert_eq!(first.meta, second.meta);
    assert_eq!(first.meta, Some(json!({ "discarded": 1 })));
}
