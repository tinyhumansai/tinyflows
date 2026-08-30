use super::*;
use crate::caps::mock::mock_capabilities;
use crate::data::Item;
use crate::model::{Node, NodeKind};
use serde_json::{Value, json};

#[tokio::test]
async fn passes_items_through_unchanged() {
    let node = Node {
        id: "p".into(),
        kind: NodeKind::OutputParser,
        type_version: 1,
        name: "p".into(),
        config: Value::Null,
        ports: vec![],
        position: None,
    };
    let input = vec![Item::new(json!({ "a": 1 })), Item::new(json!({ "b": 2 }))];
    let caps = mock_capabilities();
    let ctx = NodeContext {
        node: &node,
        input: &input,
        run: &Value::Null,
        nodes: &Value::Null,
        caps: &caps,
        agents: &[],
        observer: &crate::observability::NoopObserver,
        token: crate::engine::CancellationToken::new(),
        lane: None,
        resume: None,
        step: 0,
    };
    let out = OutputParserNode.execute(ctx).await.expect("execute");
    assert_eq!(out.items, input);
    assert_eq!(out.port, None, "passthrough stays on the main port");
}

fn parser_node() -> Node {
    Node {
        id: "p".into(),
        kind: NodeKind::OutputParser,
        type_version: 1,
        name: "p".into(),
        config: Value::Null,
        ports: vec![],
        position: None,
    }
}

async fn run_parser(input: Vec<Item>) -> Vec<Item> {
    let node = parser_node();
    let caps = mock_capabilities();
    let ctx = NodeContext {
        node: &node,
        input: &input,
        run: &Value::Null,
        nodes: &Value::Null,
        caps: &caps,
        agents: &[],
        observer: &crate::observability::NoopObserver,
        token: crate::engine::CancellationToken::new(),
        lane: None,
        resume: None,
        step: 0,
    };
    OutputParserNode.execute(ctx).await.expect("execute").items
}

#[tokio::test]
async fn passes_single_item_through() {
    let input = vec![Item::new(json!({ "only": 1 }))];
    assert_eq!(run_parser(input.clone()).await, input);
}

#[tokio::test]
async fn empty_input_yields_no_items() {
    assert!(run_parser(vec![]).await.is_empty());
}

// --- schema validation + LLM auto-fix ---

use crate::caps::{Capabilities, LlmProvider};
use async_trait::async_trait;
use std::sync::Arc;

/// An LLM that returns a canned corrected value under `value` on auto-fix.
struct FixingLlm(Value);

#[async_trait]
impl LlmProvider for FixingLlm {
    async fn complete(&self, _request: Value, _conn: Option<&str>) -> Result<Value> {
        Ok(json!({ "value": self.0.clone() }))
    }
}

/// Builds a capabilities bundle whose LLM is `llm`, everything else mocked.
fn caps_with_llm(llm: Arc<dyn LlmProvider>) -> Capabilities {
    let mut caps = mock_capabilities();
    caps.llm = llm;
    caps
}

fn schema_node(config: Value) -> Node {
    Node {
        id: "p".into(),
        kind: NodeKind::OutputParser,
        type_version: 1,
        name: "p".into(),
        config,
        ports: vec![],
        position: None,
    }
}

async fn run_with_caps(
    node: &Node,
    input: Vec<Item>,
    caps: &crate::caps::Capabilities,
) -> Result<Vec<Item>> {
    let run_meta = Value::Null;
    let ctx = NodeContext {
        node,
        input: &input,
        run: &run_meta,
        nodes: &Value::Null,
        caps,
        agents: &[],
        observer: &crate::observability::NoopObserver,
        token: crate::engine::CancellationToken::new(),
        lane: None,
        resume: None,
        step: 0,
    };
    OutputParserNode.execute(ctx).await.map(|o| o.items)
}

#[tokio::test]
async fn valid_input_passes_schema_unchanged() {
    let node = schema_node(json!({
        "schema": { "type": "object", "required": ["name"] }
    }));
    let input = vec![Item::new(json!({ "name": "A" }))];
    let caps = mock_capabilities();
    let out = run_with_caps(&node, input.clone(), &caps)
        .await
        .expect("valid");
    assert_eq!(out, input);
}

#[tokio::test]
async fn invalid_input_is_repaired_by_auto_fix() {
    let node = schema_node(json!({
        "schema": { "type": "object", "required": ["name"] }
    }));
    let input = vec![Item::new(json!({ "wrong": 1 }))];
    let caps = caps_with_llm(Arc::new(FixingLlm(json!({ "name": "fixed" }))));
    let out = run_with_caps(&node, input, &caps).await.expect("auto-fix");
    assert_eq!(out, vec![Item::new(json!({ "name": "fixed" }))]);
}

#[tokio::test]
async fn unfixable_input_errors() {
    // The default mock LLM echoes the request, so its "fix" never satisfies
    // the schema — the node surfaces a capability error.
    let node = schema_node(json!({
        "schema": { "type": "object", "required": ["name"] }
    }));
    let input = vec![Item::new(json!({ "wrong": 1 }))];
    let caps = mock_capabilities();
    let err = run_with_caps(&node, input, &caps)
        .await
        .expect_err("unfixable input must error");
    assert!(matches!(err, crate::error::EngineError::Capability(_)));
}

#[tokio::test]
async fn auto_fix_disabled_errors_on_invalid() {
    let node = schema_node(json!({
        "schema": { "type": "object", "required": ["name"] },
        "auto_fix": false
    }));
    let input = vec![Item::new(json!({ "wrong": 1 }))];
    let caps = caps_with_llm(Arc::new(FixingLlm(json!({ "name": "fixed" }))));
    let err = run_with_caps(&node, input, &caps)
        .await
        .expect_err("auto_fix=false must error");
    assert!(matches!(err, crate::error::EngineError::Capability(_)));
}

// --- end-to-end through the engine: invalid input routes per on_error ---

use crate::compiler::compile;
use crate::engine::run;
use crate::model::{Edge, WorkflowGraph};

#[tokio::test]
async fn engine_routes_unfixable_output_parser_error_via_on_error_continue() {
    // trigger -> output_parser (schema requires `name`, on_error: continue).
    // The seeded trigger item lacks `name`; the echo LLM can't fix it, so the
    // failure becomes an error item on the default port rather than failing
    // the run.
    let graph = WorkflowGraph {
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
                id: "p".into(),
                kind: NodeKind::OutputParser,
                type_version: 1,
                name: "p".into(),
                config: json!({
                    "schema": { "type": "object", "required": ["name"] },
                    "on_error": "continue"
                }),
                ports: vec![],
                position: None,
            },
        ],
        edges: vec![Edge {
            from_node: "t".into(),
            from_port: "main".into(),
            to_node: "p".into(),
            to_port: "main".into(),
        }],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let out = run(&compiled, json!({ "wrong": 1 }), &mock_capabilities())
        .await
        .expect("run completes via on_error");
    let err = &out.output["nodes"]["p"]["items"][0]["json"]["error"];
    assert_eq!(err["node"], "p");
    assert!(
        err["message"]
            .as_str()
            .unwrap()
            .contains("schema validation")
    );
}
