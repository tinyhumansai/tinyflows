use crate::caps::mock::{mock_capabilities, mock_capabilities_with_memory};
use crate::compiler::compile;
use crate::engine::run;
use crate::model::{Edge, Node, NodeKind, WorkflowGraph};
use serde_json::{Value, json};

fn wf(config: Value) -> WorkflowGraph {
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
                kind: NodeKind::Memory,
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
async fn recall_executes_against_mock_and_emits_results_on_output() {
    let graph = wf(json!({ "operation": "recall", "scope": "flow", "query": "budget" }));
    let compiled = compile(&graph).expect("compile");
    let out = run(&compiled, Value::Null, &mock_capabilities())
        .await
        .expect("run");
    let results = &out.output["nodes"]["n"]["items"][0]["json"]["json"]["results"];
    assert!(
        results.as_array().is_some_and(|r| !r.is_empty()),
        "expected shaped recall results, got {results:?}"
    );
}

#[tokio::test]
async fn flavour_operation_shapes_output() {
    let graph = wf(json!({ "operation": "flavour", "flavour": "email-tone" }));
    let compiled = compile(&graph).expect("compile");
    let out = run(&compiled, Value::Null, &mock_capabilities())
        .await
        .expect("run");
    assert_eq!(
        out.output["nodes"]["n"]["items"][0]["json"]["json"]["slug"],
        "email-tone"
    );
    assert!(out.output["nodes"]["n"]["items"][0]["json"]["json"]["traits"].is_object());
}

#[tokio::test]
async fn people_operation_shapes_output() {
    let graph = wf(json!({ "operation": "people", "query": "cyrus" }));
    let compiled = compile(&graph).expect("compile");
    let out = run(&compiled, Value::Null, &mock_capabilities())
        .await
        .expect("run");
    let people = &out.output["nodes"]["n"]["items"][0]["json"]["json"]["people"];
    assert!(people.as_array().is_some_and(|p| !p.is_empty()));
}

#[tokio::test]
async fn remember_flow_scope_writes_via_provider_and_passes_items_through() {
    let graph = wf(json!({
        "operation": "remember", "scope": "flow", "key": "k1", "value": { "v": 1 }
    }));
    let compiled = compile(&graph).expect("compile");
    let out = run(&compiled, Value::Null, &mock_capabilities())
        .await
        .expect("run");
    assert_eq!(
        out.output["nodes"]["n"]["items"][0]["json"]["json"]["ok"],
        true
    );
    assert_eq!(
        out.output["nodes"]["n"]["items"][0]["json"]["json"]["key"],
        "k1"
    );
}

#[tokio::test]
async fn forget_flow_scope_executes() {
    let graph = wf(json!({ "operation": "forget", "scope": "flow", "key": "k1" }));
    let compiled = compile(&graph).expect("compile");
    let out = run(&compiled, Value::Null, &mock_capabilities())
        .await
        .expect("run");
    assert_eq!(
        out.output["nodes"]["n"]["items"][0]["json"]["json"]["operation"],
        "forget"
    );
}

use super::MemoryNode;
use crate::data::Item;
use crate::error::EngineError;
use crate::nodes::{NodeContext, NodeExecutor};

fn memory_node(config: Value) -> Node {
    Node {
        id: "n".into(),
        kind: NodeKind::Memory,
        type_version: 1,
        name: "n".into(),
        config,
        ports: vec![],
        position: None,
    }
}

#[tokio::test]
async fn resolves_query_expression_per_item() {
    // `query="=item.id"` must resolve per-item before the provider call —
    // the `split_out` → `memory[recall]` dedupe pattern depends on this.
    let node = memory_node(json!({
        "operation": "recall", "scope": "flow", "query": "=item.id"
    }));
    let input = vec![
        Item::new(json!({ "id": "a" })),
        Item::new(json!({ "id": "b" })),
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
    let out = MemoryNode.execute(ctx).await.expect("execute");
    assert_eq!(out.items.len(), 2, "per_item default maps over input");
    assert_eq!(out.items[0].json["json"]["query"], "a");
    assert_eq!(out.items[1].json["json"]["query"], "b");
    assert_eq!(out.items[1].paired_item, Some(1));
}

#[tokio::test]
async fn search_operation_passes_operation_through_opts() {
    // `search` reuses `recall` but tags `opts.operation` so a host that
    // distinguishes semantic recall from full-text search can branch on it.
    struct OptsEchoingMemory;
    #[async_trait::async_trait]
    impl crate::caps::MemoryProvider for OptsEchoingMemory {
        async fn recall(
            &self,
            _scope: &str,
            _query: &str,
            opts: Value,
        ) -> crate::error::Result<Value> {
            Ok(json!({ "opts": opts }))
        }
        async fn flavour(&self, _slug: &str) -> crate::error::Result<Value> {
            Ok(Value::Null)
        }
        async fn people(&self, _query: Option<&str>) -> crate::error::Result<Value> {
            Ok(Value::Null)
        }
        async fn remember(
            &self,
            _scope: &str,
            _key: &str,
            _value: Value,
        ) -> crate::error::Result<()> {
            Ok(())
        }
        async fn forget(&self, _scope: &str, _key: &str) -> crate::error::Result<()> {
            Ok(())
        }
    }

    let node = memory_node(json!({
        "operation": "search", "scope": "flow", "query": "x", "limit": 5
    }));
    let input: Vec<Item> = vec![];
    let caps = mock_capabilities_with_memory(OptsEchoingMemory);
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
    let out = MemoryNode.execute(ctx).await.expect("execute");
    assert_eq!(out.items[0].json["json"]["opts"]["operation"], "search");
    assert_eq!(out.items[0].json["json"]["opts"]["limit"], 5);
}

#[tokio::test]
async fn missing_query_on_recall_is_a_capability_error() {
    let node = memory_node(json!({ "operation": "recall", "scope": "flow" }));
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
    let err = MemoryNode
        .execute(ctx)
        .await
        .expect_err("missing query must error");
    assert!(
        matches!(err, EngineError::Capability(ref m) if m.contains("query")),
        "expected a capability error mentioning `query`, got: {err:?}"
    );
}

#[tokio::test]
async fn missing_key_on_remember_is_a_capability_error() {
    let node = memory_node(json!({
        "operation": "remember", "scope": "flow", "value": 1
    }));
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
    let err = MemoryNode
        .execute(ctx)
        .await
        .expect_err("missing key must error");
    assert!(
        matches!(err, EngineError::Capability(ref m) if m.contains("key")),
        "expected a capability error mentioning `key`, got: {err:?}"
    );
}

#[tokio::test]
async fn write_to_non_flow_scope_is_a_capability_error_even_when_driven_directly() {
    // The validator rejects non-"flow" writes, but the executor is the last
    // line of defense when driven directly (bypassing validate). A
    // remember/forget against "flows" (read-only) or an absent scope must
    // hard-error, never silently write to the wrong place.
    for (op, cfg) in [
        (
            "remember",
            json!({ "operation": "remember", "scope": "flows", "key": "k", "value": 1 }),
        ),
        (
            "forget",
            json!({ "operation": "forget", "scope": "flows", "key": "k" }),
        ),
        // absent scope defaults to "" — also not "flow" — and must error.
        (
            "remember",
            json!({ "operation": "remember", "key": "k", "value": 1 }),
        ),
    ] {
        let node = memory_node(cfg);
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
        let err = MemoryNode
            .execute(ctx)
            .await
            .expect_err("non-flow write must error");
        assert!(
            matches!(err, EngineError::Capability(ref m) if m.contains("flow") && m.contains(op)),
            "expected a capability error mentioning `flow` and `{op}`, got: {err:?}"
        );
    }
}

#[tokio::test]
async fn missing_operation_is_a_capability_error() {
    let node = memory_node(json!({ "scope": "flow" }));
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
    let err = MemoryNode
        .execute(ctx)
        .await
        .expect_err("missing operation must error");
    assert!(
        matches!(err, EngineError::Capability(ref m) if m.contains("operation")),
        "expected a capability error mentioning `operation`, got: {err:?}"
    );
}

#[tokio::test]
async fn no_memory_provider_wired_is_a_capability_error() {
    let node = memory_node(json!({ "operation": "people" }));
    let input = vec![];
    let mut caps = mock_capabilities();
    caps.memory = None;
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
    let err = MemoryNode
        .execute(ctx)
        .await
        .expect_err("no MemoryProvider must error");
    assert!(
        matches!(err, EngineError::Capability(ref m) if m.contains("MemoryProvider")),
        "expected a capability error mentioning MemoryProvider, got: {err:?}"
    );
}

#[tokio::test]
async fn execution_once_collapses_the_batch_to_a_single_call() {
    let node = memory_node(json!({
        "operation": "recall", "scope": "flow", "query": "=item.id", "execution": "once"
    }));
    let input = vec![
        Item::new(json!({ "id": "a" })),
        Item::new(json!({ "id": "b" })),
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
    let out = MemoryNode.execute(ctx).await.expect("execute");
    assert_eq!(out.items.len(), 1, "once mode emits a single item");
    assert_eq!(out.items[0].json["json"]["query"], "a");
}
