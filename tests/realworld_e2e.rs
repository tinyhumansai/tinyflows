#![cfg(feature = "mock")]
//! Realistic end-to-end workflows that assert the OUTCOME a user expects:
//! per-row messaging, an AI draft handed to a post action, an approval-gated
//! action, and a small ETL branch+merge. Failing ones (bug reproductions) are
//! marked `#[ignore]`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tinyflows::caps::mock::mock_capabilities;
use tinyflows::caps::{Capabilities, LlmProvider};
use tinyflows::compiler::compile;
use tinyflows::engine::{run, run_resumable};
use tinyflows::model::{Edge, Node, NodeKind, WorkflowGraph};

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
    node(id, NodeKind::Trigger, Value::Null)
}

fn edge(from_node: &str, from_port: &str, to_node: &str) -> Edge {
    Edge {
        from_node: from_node.to_string(),
        from_port: from_port.to_string(),
        to_node: to_node.to_string(),
        to_port: "main".to_string(),
    }
}

/// North-star: "send a message per row". A `split_out` fans an array into N
/// items and a `tool_call` (default per-item) must fire once per item — not
/// once against item[0].
#[tokio::test]
async fn email_per_row_sends_once_per_row() {
    let graph = WorkflowGraph {
        name: "email_per_row".to_string(),
        nodes: vec![
            trigger("start"),
            node("split", NodeKind::SplitOut, json!({ "path": "rows" })),
            node(
                "send",
                NodeKind::ToolCall,
                json!({ "slug": "email.send", "args": { "to": "=item.to" } }),
            ),
        ],
        edges: vec![
            edge("start", "main", "split"),
            edge("split", "main", "send"),
        ],
        ..Default::default()
    };

    let compiled = compile(&graph).expect("compile");
    let outcome = run(
        &compiled,
        json!({ "rows": [{ "to": "a" }, { "to": "b" }, { "to": "c" }] }),
        &mock_capabilities(),
    )
    .await
    .expect("run");

    let items = outcome.output["nodes"]["send"]["items"]
        .as_array()
        .expect("send emitted items");
    assert_eq!(items.len(), 3, "one send per row (per-item execution)");
    let tos: Vec<&str> = items
        .iter()
        .map(|i| i["json"]["json"]["args"]["to"].as_str().unwrap_or("?"))
        .collect();
    assert_eq!(tos, ["a", "b", "c"]);
}

/// An LLM that answers in prose (a `{text: …}` completion), so the agent
/// envelope's `text` accessor is populated.
struct ProseLlm;

#[async_trait]
impl LlmProvider for ProseLlm {
    async fn complete(
        &self,
        _request: Value,
        _conn: Option<&str>,
    ) -> tinyflows::error::Result<Value> {
        Ok(json!({ "text": "drafted message" }))
    }
}

fn caps_with_prose_llm() -> Capabilities {
    Capabilities {
        llm: Arc::new(ProseLlm),
        ..mock_capabilities()
    }
}

/// "AI draft -> post": an `agent` drafts prose and a downstream `tool_call`
/// posts it, reading the draft via the stable envelope accessor
/// `=nodes.draft.item.text`.
#[tokio::test]
async fn ai_draft_then_post_flows_text_through_envelope() {
    let graph = WorkflowGraph {
        name: "draft_post".to_string(),
        nodes: vec![
            trigger("start"),
            node("draft", NodeKind::Agent, json!({ "prompt": "=item.name" })),
            node(
                "post",
                NodeKind::ToolCall,
                json!({ "slug": "slack.post", "args": { "text": "=nodes.draft.item.text" } }),
            ),
        ],
        edges: vec![
            edge("start", "main", "draft"),
            edge("draft", "main", "post"),
        ],
        ..Default::default()
    };

    let compiled = compile(&graph).expect("compile");
    let outcome = run(&compiled, json!({ "name": "Ada" }), &caps_with_prose_llm())
        .await
        .expect("run");
    let out = &outcome.output;

    // The agent envelope exposes text/json/raw; the prose lands under `text`.
    let draft = &out["nodes"]["draft"]["items"][0]["json"];
    assert_eq!(
        draft["text"], "drafted message",
        "agent envelope `text` accessor"
    );
    assert!(
        draft.get("json").is_some() && draft.get("raw").is_some(),
        "envelope keys present"
    );

    // The tool_call read the agent's text via the cross-node envelope accessor.
    assert_eq!(
        out["nodes"]["post"]["items"][0]["json"]["json"]["args"]["text"], "drafted message",
        "post should receive the agent's text via =nodes.draft.item.text"
    );
}

/// "Approval-gated action": a `tool_call` gate with `requires_approval` pauses
/// the run, its downstream does not run, and a resume completes it.
#[tokio::test]
async fn approval_gated_action_pauses_then_resumes() {
    let graph = WorkflowGraph {
        name: "approval".to_string(),
        nodes: vec![
            trigger("start"),
            node(
                "gate",
                NodeKind::ToolCall,
                json!({ "slug": "pay", "requires_approval": true }),
            ),
            node("confirm", NodeKind::OutputParser, Value::Null),
        ],
        edges: vec![
            edge("start", "main", "gate"),
            edge("gate", "main", "confirm"),
        ],
        ..Default::default()
    };

    let compiled = compile(&graph).expect("compile");
    let caps = mock_capabilities();

    let resumable = run_resumable(&compiled, json!({ "amount": 50 }), &caps)
        .await
        .expect("run");
    assert_eq!(
        resumable.outcome().pending_approvals,
        vec!["gate".to_string()],
        "the gate should be pending approval"
    );
    assert!(
        resumable.outcome().output["nodes"]["confirm"].is_null(),
        "confirm must not run before approval"
    );

    let resumed = resumable
        .resume(vec!["gate".to_string()])
        .await
        .expect("resume");
    assert!(
        resumed.pending_approvals.is_empty(),
        "no gates pending after approval"
    );
    assert!(
        !resumed.output["nodes"]["confirm"].is_null(),
        "confirm should run after the gate is approved"
    );
}

/// "ETL branch+merge": fetch -> shape -> split -> condition -> {tag | skip} ->
/// merge. Asserts the merged item count and a computed tier field. Wrapped in a
/// hard timeout so a merge-barrier deadlock can never hang CI.
#[tokio::test]
#[ignore = "BUG-4: a `merge` fed by both a condition's true and false branch nodes gets an incomplete waiting-edge barrier; the untaken `skip` predecessor never completes so `collect` never fires (no run slot), losing all merged items"]
async fn etl_branch_merge_collects_tagged_items() {
    let graph = WorkflowGraph {
        name: "etl".to_string(),
        nodes: vec![
            trigger("start"),
            node(
                "fetch",
                NodeKind::HttpRequest,
                json!({ "method": "GET", "url": "https://api/x" }),
            ),
            // Carry the trigger's orders forward (the http fetch is a side fetch).
            node(
                "shape",
                NodeKind::Transform,
                json!({ "set": { "orders": "=run.trigger.orders" } }),
            ),
            node("split", NodeKind::SplitOut, json!({ "path": "orders" })),
            node("level", NodeKind::Condition, json!({ "field": "amount" })),
            node(
                "tag",
                NodeKind::Transform,
                json!({ "set": { "tier": "gold" } }),
            ),
            node(
                "skip",
                NodeKind::Transform,
                json!({ "set": { "tier": "none" } }),
            ),
            node("collect", NodeKind::Merge, Value::Null),
        ],
        edges: vec![
            edge("start", "main", "fetch"),
            edge("fetch", "main", "shape"),
            edge("shape", "main", "split"),
            edge("split", "main", "level"),
            edge("level", "true", "tag"),
            edge("level", "false", "skip"),
            edge("tag", "main", "collect"),
            edge("skip", "main", "collect"),
        ],
        ..Default::default()
    };

    let compiled = compile(&graph).expect("compile");
    let caps = mock_capabilities();

    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        run(
            &compiled,
            json!({ "orders": [{ "amount": 150 }, { "amount": 200 }] }),
            &caps,
        ),
    )
    .await
    .expect("ETL run must not hang")
    .expect("run");

    let items = outcome.output["nodes"]["collect"]["items"]
        .as_array()
        .expect("collect emitted items");
    assert_eq!(items.len(), 2, "both orders should reach the merge");
    for item in items {
        assert_eq!(
            item["json"]["tier"], "gold",
            "amount>0 routes to the gold tier"
        );
    }
}
