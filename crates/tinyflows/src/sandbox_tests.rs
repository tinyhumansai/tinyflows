use super::*;

use crate::caps::{ToolInvoker, mock};
use crate::error::Result;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

struct PendingTool;

#[async_trait]
impl ToolInvoker for PendingTool {
    async fn invoke(&self, _: &str, _: Value, _: Option<&str>) -> Result<Value> {
        std::future::pending().await
    }
}

#[tokio::test]
async fn sandbox_execution_stops_at_its_deadline() {
    let graph = crate::migrate::deserialize_graph(json!({
        "nodes":[
            {"id":"trigger","name":"Trigger","kind":"trigger","config":{}},
            {"id":"invoke","name":"Invoke","kind":"tool_call","config":{"slug":"demo:echo","args":{}}}
        ],
        "edges":[{"from_node":"trigger","to_node":"invoke"}]
    }))
    .unwrap();
    let compiled = crate::compiler::compile(&graph).unwrap();
    let mut capabilities = mock::mock_capabilities();
    capabilities.tools = Arc::new(PendingTool);

    let error = execute(
        &graph,
        &compiled,
        json!({}),
        &capabilities,
        Duration::from_millis(1),
    )
    .await
    .unwrap_err();

    assert!(matches!(error, SandboxError::TimedOut));
}

async fn dry_run_code(source: &str) -> Value {
    let graph = crate::migrate::deserialize_graph(json!({
        "nodes":[
            {"id":"trigger","name":"Trigger","kind":"trigger","config":{}},
            {"id":"shape","name":"Shape","kind":"code","config":{"language":"javascript","source":source}}
        ],
        "edges":[{"from_node":"trigger","to_node":"shape"}]
    }))
    .unwrap();
    let compiled = crate::compiler::compile(&graph).unwrap();
    let execution = execute(
        &graph,
        &compiled,
        json!({}),
        &mock::mock_capabilities(),
        Duration::from_secs(5),
    )
    .await;
    report(execution, 5, vec![]).unwrap()
}

#[tokio::test]
async fn a_clean_dry_run_says_its_code_nodes_never_ran() {
    let result = dry_run_code("return { n: input.length };").await;
    assert_eq!(result["ok"], true);
    assert_eq!(result["unexecuted_code_nodes"][0]["node_id"], "shape");
    assert_eq!(result["code_source_errors"], json!([]));
    assert!(
        result["note"]
            .as_str()
            .unwrap()
            .starts_with("CODE NOT EXECUTED: 1 code node(s) (shape)")
    );
}

#[tokio::test]
async fn a_flagged_item_read_is_a_hint_and_does_not_fail_the_dry_run() {
    // Valid code: `item` is bound by rename-destructuring, which the token
    // scan cannot see.
    let result = dry_run_code("const { json: item } = input[0];\nreturn item;").await;
    assert_eq!(result["ok"], true);
    assert_eq!(result["code_source_errors"][0]["node_id"], "shape");
    let note = result["note"].as_str().unwrap();
    assert!(note.starts_with("CODE NOT EXECUTED"));
    assert!(note.contains("POSSIBLE UNDEFINED `item` in code node(s) (shape)"));
}

#[test]
fn a_graph_without_code_nodes_gets_no_code_summary() {
    assert_eq!(code_summary(&[], &[]), "");
}
