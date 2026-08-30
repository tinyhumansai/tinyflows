#![cfg(feature = "mock")]
//! Wire-format reference workflows for the asynchronous and lane node pairs.
//!
//! These intentionally start as JSON, so a renamed discriminator or config key
//! fails before execution rather than being hidden by Rust constructors.

use std::time::Duration;

use serde_json::{Value, json};
use tinyflows::caps::mock::mock_capabilities;
use tinyflows::compiler::compile;
use tinyflows::engine::run;
use tinyflows::model::WorkflowGraph;

const GUARD: Duration = Duration::from_secs(5);

async fn load_compile_run(document: Value, input: Value) -> tinyflows::engine::RunOutcome {
    let graph: WorkflowGraph = serde_json::from_value(document).expect("reference JSON loads");
    let encoded = serde_json::to_value(&graph).expect("reference graph serializes");
    let decoded: WorkflowGraph = serde_json::from_value(encoded).expect("round trip loads");
    assert_eq!(decoded, graph, "published workflow shape must round-trip");
    let compiled = compile(&graph).expect("reference workflow compiles");
    tokio::time::timeout(GUARD, run(&compiled, input, &mock_capabilities()))
        .await
        .expect("reference workflow hung")
        .expect("reference workflow runs")
}

#[tokio::test]
async fn spawn_and_gate_reference_workflow() {
    let outcome = load_compile_run(
        json!({
            "schema_version": 1,
            "name": "parallel background research",
            "nodes": [
                { "id": "start", "kind": "trigger", "type_version": 1,
                  "name": "start", "config": { "recursion_limit": 100 } },
                { "id": "fanout", "kind": "output_parser", "type_version": 1,
                  "name": "fanout", "config": null },
                { "id": "search", "kind": "spawn", "type_version": 1,
                  "name": "search", "config": {
                      "target": "tool", "slug": "research.search", "args": { "q": "rust" }
                  } },
                { "id": "summarize", "kind": "spawn", "type_version": 1,
                  "name": "summarize", "config": {
                      "target": "http", "request": { "url": "https://example.test/summary" }
                  } },
                { "id": "ready", "kind": "gate", "type_version": 1,
                  "name": "ready", "config": {
                      "from": ["search", "summarize"], "release": "all",
                      "poll_interval_ms": 1
                  } }
            ],
            "edges": [
                { "from_node": "start", "from_port": "main", "to_node": "fanout", "to_port": "main" },
                { "from_node": "fanout", "from_port": "main", "to_node": "search", "to_port": "main" },
                { "from_node": "fanout", "from_port": "main", "to_node": "summarize", "to_port": "main" },
                { "from_node": "search", "from_port": "main", "to_node": "ready", "to_port": "main" },
                { "from_node": "summarize", "from_port": "main", "to_node": "ready", "to_port": "main" }
            ]
        }),
        json!({}),
    )
    .await;

    let items = outcome.output["nodes"]["ready"]["items"]
        .as_array()
        .expect("gate items");
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["json"]["spec"], "tool");
    assert_eq!(items[1]["json"]["spec"], "http");
}

#[tokio::test]
async fn scatter_and_gather_reference_workflow() {
    let outcome = load_compile_run(
        json!({
            "schema_version": 1,
            "name": "parallel row enrichment",
            "nodes": [
                { "id": "start", "kind": "trigger", "type_version": 1,
                  "name": "start", "config": { "max_concurrency": 2, "recursion_limit": 100 } },
                { "id": "rows", "kind": "scatter", "type_version": 1,
                  "name": "rows", "config": { "path": "rows" } },
                { "id": "enrich", "kind": "transform", "type_version": 1,
                  "name": "enrich", "config": { "set": { "enriched": true } } },
                { "id": "all_rows", "kind": "gather", "type_version": 1,
                  "name": "all_rows", "config": {
                      "from": ["enrich"], "release": "all", "poll_interval_ms": 1
                  } }
            ],
            "edges": [
                { "from_node": "start", "from_port": "main", "to_node": "rows", "to_port": "main" },
                { "from_node": "rows", "from_port": "main", "to_node": "enrich", "to_port": "main" },
                { "from_node": "enrich", "from_port": "main", "to_node": "all_rows", "to_port": "main" }
            ]
        }),
        json!({ "rows": [{"id": 1}, {"id": 2}, {"id": 3}] }),
    )
    .await;

    let items = outcome.output["nodes"]["all_rows"]["items"]
        .as_array()
        .expect("gather items");
    assert_eq!(items.len(), 3);
    assert!(items.iter().all(|item| item["json"]["enriched"] == true));
}
