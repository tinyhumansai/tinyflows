use crate::caps::mock::mock_capabilities;
use crate::compiler::compile;
use crate::engine::run;
use crate::model::{Edge, Node, NodeKind, WorkflowGraph};
use serde_json::{Value, json};

fn wf(kind: NodeKind, config: Value) -> WorkflowGraph {
    WorkflowGraph {
        nodes: vec![
            Node {
                id: "t".into(),
                kind: NodeKind::Trigger,
                type_version: 1,
                name: "t".into(),
                config: Value::Null,
                ports: vec![],
                position: None,
            },
            Node {
                id: "n".into(),
                kind,
                type_version: 1,
                name: "n".into(),
                config,
                ports: vec![],
                position: None,
            },
        ],
        edges: vec![Edge {
            from_node: "t".into(),
            from_port: "main".into(),
            to_node: "n".into(),
            to_port: "main".into(),
        }],
        ..Default::default()
    }
}

#[tokio::test]
async fn tool_call_invokes_slug_with_args() {
    let graph = wf(
        NodeKind::ToolCall,
        json!({ "slug": "slack.post", "args": { "x": 1 } }),
    );
    let compiled = compile(&graph).expect("compile");
    let out = run(&compiled, Value::Null, &mock_capabilities())
        .await
        .expect("run");
    // Output is enveloped: the tool result is under `json.*` (`raw` keeps it too).
    assert_eq!(
        out.output["nodes"]["n"]["items"][0]["json"]["json"]["tool"],
        "slack.post"
    );
    assert_eq!(
        out.output["nodes"]["n"]["items"][0]["json"]["json"]["args"]["x"],
        1
    );
}

#[tokio::test]
async fn tool_call_threads_connection_ref() {
    let graph = wf(
        NodeKind::ToolCall,
        json!({ "slug": "slack.post", "connection_ref": "composio:slack:acct_1" }),
    );
    let compiled = compile(&graph).expect("compile");
    let out = run(&compiled, Value::Null, &mock_capabilities())
        .await
        .expect("run");
    assert_eq!(
        out.output["nodes"]["n"]["items"][0]["json"]["json"]["connection"],
        "composio:slack:acct_1"
    );
}

use super::ToolCallNode;
use crate::data::Item;
use crate::error::EngineError;
use crate::nodes::{NodeContext, NodeExecutor};

fn tool_node(config: Value) -> Node {
    Node {
        id: "n".into(),
        kind: NodeKind::ToolCall,
        type_version: 1,
        name: "n".into(),
        config,
        ports: vec![],
        position: None,
    }
}

#[tokio::test]
async fn missing_slug_is_a_capability_error() {
    let node = tool_node(json!({ "args": { "x": 1 } }));
    let input = vec![];
    let caps = mock_capabilities();
    let run_meta = Value::Null;
    let ctx = NodeContext {
        node: &node,
        input: &input,
        run: &run_meta,
        nodes: &Value::Null,
        caps: &caps,
        agents: &[],
        observer: &crate::observability::NoopObserver,
        token: crate::engine::CancellationToken::new(),
        lane: None,
        resume: None,
        step: 0,
    };
    let err = ToolCallNode
        .execute(ctx)
        .await
        .expect_err("missing slug must error");
    assert!(
        matches!(err, EngineError::Capability(ref m) if m.contains("slug")),
        "expected a capability error mentioning `slug`, got: {err:?}"
    );
}

#[tokio::test]
async fn resolves_expression_in_args_against_input() {
    // `args.text` is a `=`-expression that must bind to the input item's
    // `name`; the mock tool echoes the args it was invoked with.
    let node = tool_node(json!({ "slug": "x.y", "args": { "text": "=item.name" } }));
    let input = vec![Item::new(json!({ "name": "X" }))];
    let caps = mock_capabilities();
    let run_meta = Value::Null;
    let ctx = NodeContext {
        node: &node,
        input: &input,
        run: &run_meta,
        nodes: &Value::Null,
        caps: &caps,
        agents: &[],
        observer: &crate::observability::NoopObserver,
        token: crate::engine::CancellationToken::new(),
        lane: None,
        resume: None,
        step: 0,
    };
    let out = ToolCallNode.execute(ctx).await.expect("execute");
    assert_eq!(out.items[0].json["json"]["tool"], "x.y");
    assert_eq!(out.items[0].json["json"]["args"]["text"], "X");
    // Per-item execution pairs each output back to its input index.
    assert_eq!(out.items[0].paired_item, Some(0));
}

#[tokio::test]
async fn null_resolved_expression_is_reported_in_diagnostics() {
    // `args.to` misses (the input has no `email`); the node still runs but
    // its output carries a diagnostic naming the location and expression.
    let node = tool_node(json!({ "slug": "gmail.send", "args": {
            "text": "=item.name", "to": "=item.email"
        } }));
    let input = vec![Item::new(json!({ "name": "X" }))];
    let caps = mock_capabilities();
    let run_meta = Value::Null;
    let ctx = NodeContext {
        node: &node,
        input: &input,
        run: &run_meta,
        nodes: &Value::Null,
        caps: &caps,
        agents: &[],
        observer: &crate::observability::NoopObserver,
        token: crate::engine::CancellationToken::new(),
        lane: None,
        resume: None,
        step: 0,
    };
    let out = ToolCallNode.execute(ctx).await.expect("execute");
    assert_eq!(out.items[0].json["json"]["args"]["to"], Value::Null);
    assert_eq!(out.diagnostics.len(), 1);
    assert_eq!(out.diagnostics[0].location, "args.to");
    assert_eq!(out.diagnostics[0].expression, "=item.email");
}

#[tokio::test]
async fn missing_args_default_to_null() {
    let node = tool_node(json!({ "slug": "noop" }));
    let input = vec![];
    let caps = mock_capabilities();
    let run_meta = Value::Null;
    let ctx = NodeContext {
        node: &node,
        input: &input,
        run: &run_meta,
        nodes: &Value::Null,
        caps: &caps,
        agents: &[],
        observer: &crate::observability::NoopObserver,
        token: crate::engine::CancellationToken::new(),
        lane: None,
        resume: None,
        step: 0,
    };
    let out = ToolCallNode.execute(ctx).await.expect("execute");
    assert_eq!(out.items.len(), 1);
    assert_eq!(out.items[0].json["json"]["tool"], "noop");
    assert_eq!(out.items[0].json["json"]["args"], Value::Null);
    assert_eq!(out.items[0].json["json"]["connection"], Value::Null);
}

#[tokio::test]
async fn per_item_execution_maps_over_the_input() {
    // Default `per_item`: a tool_call fed N items invokes N times, one output
    // per input, with config re-resolved against each item — the fan-out fix.
    let node = tool_node(json!({ "slug": "x.y", "args": { "text": "=item.name" } }));
    let input = vec![
        Item::new(json!({ "name": "A" })),
        Item::new(json!({ "name": "B" })),
        Item::new(json!({ "name": "C" })),
    ];
    let caps = mock_capabilities();
    let run_meta = Value::Null;
    let ctx = NodeContext {
        node: &node,
        input: &input,
        run: &run_meta,
        nodes: &Value::Null,
        caps: &caps,
        agents: &[],
        observer: &crate::observability::NoopObserver,
        token: crate::engine::CancellationToken::new(),
        lane: None,
        resume: None,
        step: 0,
    };
    let out = ToolCallNode.execute(ctx).await.expect("execute");
    assert_eq!(out.items.len(), 3, "one output per input item");
    assert_eq!(out.items[0].json["json"]["args"]["text"], "A");
    assert_eq!(out.items[1].json["json"]["args"]["text"], "B");
    assert_eq!(out.items[2].json["json"]["args"]["text"], "C");
    assert_eq!(out.items[2].paired_item, Some(2));
}

#[tokio::test]
async fn execution_once_collapses_the_batch_to_a_single_call() {
    // Opt-out: `execution: "once"` invokes a single time against the first item.
    let node = tool_node(json!({
        "slug": "x.y", "args": { "text": "=item.name" }, "execution": "once"
    }));
    let input = vec![
        Item::new(json!({ "name": "A" })),
        Item::new(json!({ "name": "B" })),
    ];
    let caps = mock_capabilities();
    let run_meta = Value::Null;
    let ctx = NodeContext {
        node: &node,
        input: &input,
        run: &run_meta,
        nodes: &Value::Null,
        caps: &caps,
        agents: &[],
        observer: &crate::observability::NoopObserver,
        token: crate::engine::CancellationToken::new(),
        lane: None,
        resume: None,
        step: 0,
    };
    let out = ToolCallNode.execute(ctx).await.expect("execute");
    assert_eq!(out.items.len(), 1, "once mode emits a single item");
    assert_eq!(out.items[0].json["json"]["args"]["text"], "A");
}
