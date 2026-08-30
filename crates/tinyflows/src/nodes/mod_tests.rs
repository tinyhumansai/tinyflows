use super::*;
use crate::caps::mock::mock_capabilities;
use crate::data::Item;
use crate::model::Node;
use serde_json::json;

/// Every [`NodeKind`] variant, so the coverage below stays exhaustive.
fn all_kinds() -> Vec<NodeKind> {
    use NodeKind::{
        Agent, Code, Condition, Dedup, HttpRequest, Loop, Memory, Merge, OutputParser, Shell,
        SplitOut, SubWorkflow, Switch, ToolCall, Transform, Trigger, Void,
    };
    vec![
        Trigger,
        Agent,
        ToolCall,
        HttpRequest,
        Code,
        Shell,
        Condition,
        Switch,
        Merge,
        SplitOut,
        Transform,
        OutputParser,
        SubWorkflow,
        Memory,
        Dedup,
        Loop,
        Void,
    ]
}

/// Minimal config that lets each kind execute successfully.
fn config_for(kind: &NodeKind) -> Value {
    match kind {
        NodeKind::ToolCall => json!({ "slug": "demo" }),
        NodeKind::Shell => json!({ "source": "printf ok" }),
        NodeKind::SubWorkflow => json!({
            "workflow": { "nodes": [{ "id": "ct", "kind": "trigger", "name": "ct" }], "edges": [] }
        }),
        // `people` needs no `scope`/`query`, so it runs against the
        // default mock capabilities (which wire a `MemoryProvider`) with
        // the minimal config every other kind gets via `Value::Null`.
        NodeKind::Memory => json!({ "operation": "people" }),
        _ => Value::Null,
    }
}

fn node(kind: NodeKind, config: Value) -> Node {
    Node {
        id: "n".into(),
        kind,
        type_version: 1,
        name: "n".into(),
        config,
        ports: vec![],
        position: None,
    }
}

#[tokio::test]
async fn executor_for_is_total_and_every_executor_runs() {
    let caps = mock_capabilities();
    let run = Value::Null;
    for kind in all_kinds() {
        let node = node(kind.clone(), config_for(&kind));
        let input = vec![Item::new(json!({ "x": 1 }))];
        let exec = executor_for(&kind);
        let out = exec
            .execute(NodeContext {
                node: &node,
                input: &input,
                run: &run,
                nodes: &Value::Null,
                caps: &caps,
                agents: &[],
                observer: &crate::observability::NoopObserver,
                token: crate::engine::CancellationToken::new(),
                lane: None,
                resume: None,
                step: 0,
            })
            .await;
        assert!(
            out.is_ok(),
            "executor for {kind:?} should run: {:?}",
            out.err()
        );
    }
}

#[tokio::test]
async fn trigger_executor_passes_input_through() {
    let caps = mock_capabilities();
    let run = Value::Null;
    let node = node(NodeKind::Trigger, Value::Null);
    let input = vec![Item::new(json!({ "a": 1 })), Item::new(json!({ "b": 2 }))];
    let out = executor_for(&NodeKind::Trigger)
        .execute(NodeContext {
            node: &node,
            input: &input,
            run: &run,
            nodes: &Value::Null,
            caps: &caps,
            agents: &[],
            observer: &crate::observability::NoopObserver,
            token: crate::engine::CancellationToken::new(),
            lane: None,
            resume: None,
            step: 0,
        })
        .await
        .expect("execute");
    assert_eq!(out.items, input);
    assert_eq!(out.port, None);
}

#[test]
fn expr_scope_exposes_completed_nodes_keyed_by_id() {
    let caps = mock_capabilities();
    let run = Value::Null;
    let n = node(NodeKind::Transform, Value::Null);
    let input = vec![Item::new(json!({ "in": 1 }))];
    // Run-state shape: serialized `Item`s under each completed node's slot.
    let nodes_state = json!({
        "a": { "items": [
            { "json": { "x": 42 } },
            { "json": { "x": 43 }, "paired_item": 0 },
        ] },
        "b": { "items": [], "port": "true" },
        "broken": { "no_items": true },
    });
    let ctx = NodeContext {
        node: &n,
        input: &input,
        run: &run,
        nodes: &nodes_state,
        caps: &caps,
        agents: &[],
        observer: &crate::observability::NoopObserver,
        token: crate::engine::CancellationToken::new(),
        lane: None,
        resume: None,
        step: 0,
    };
    let scope = expr_scope(&ctx);
    // Existing keys unchanged (back-compat).
    assert_eq!(scope["item"], json!({ "in": 1 }));
    assert_eq!(scope["items"], json!([{ "in": 1 }]));
    // `nodes.<id>` projects each slot's item `json` payloads.
    assert_eq!(scope["nodes"]["a"]["item"], json!({ "x": 42 }));
    assert_eq!(
        scope["nodes"]["a"]["items"],
        json!([{ "x": 42 }, { "x": 43 }])
    );
    // An empty slot yields a null `item` and empty `items`.
    assert_eq!(scope["nodes"]["b"]["item"], Value::Null);
    assert_eq!(scope["nodes"]["b"]["items"], json!([]));
    // A slot without an `items` array is skipped, not panicked on.
    assert!(scope["nodes"].get("broken").is_none());
}

#[test]
fn expr_scope_with_null_nodes_state_is_empty_map() {
    let caps = mock_capabilities();
    let run = Value::Null;
    let n = node(NodeKind::Transform, Value::Null);
    let ctx = NodeContext {
        node: &n,
        input: &[],
        run: &run,
        nodes: &Value::Null,
        caps: &caps,
        agents: &[],
        observer: &crate::observability::NoopObserver,
        token: crate::engine::CancellationToken::new(),
        lane: None,
        resume: None,
        step: 0,
    };
    let scope = expr_scope(&ctx);
    assert_eq!(scope["nodes"], json!({}));
}

#[test]
fn node_output_constructors_have_expected_shapes() {
    let items = vec![Item::new(json!({ "a": 1 }))];

    let main = NodeOutput::main(items.clone());
    assert_eq!(main.port, None);
    assert_eq!(main.items, items);

    let routed = NodeOutput::routed(items.clone(), "true");
    assert_eq!(routed.port.as_deref(), Some("true"));
    assert_eq!(routed.items, items);

    let empty = NodeOutput::empty();
    assert!(empty.items.is_empty());
    assert_eq!(empty.port, None);
}
