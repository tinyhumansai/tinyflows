#![cfg(feature = "mock")]
//! End-to-end test for the load-time migration path of legacy workflow JSON.
//!
//! A definition persisted before the versioning fields existed carries no
//! top-level `schema_version` and no per-node `type_version`. The load path is
//! `raw JSON -> migrate -> from_value::<WorkflowGraph> -> compile -> run`
//! (see `src/migrate.rs`). This test feeds such a legacy value through that whole
//! path and asserts the serde/migration defaults were applied and the run
//! completes end to end.
//!
//! Gated behind the `mock` feature, so plain `cargo test` skips it while
//! `cargo test --all-features` runs it.

use serde_json::json;
use tinyflows::caps::mock::mock_capabilities;
use tinyflows::compiler::compile;
use tinyflows::engine::run;
use tinyflows::migrate::migrate;
use tinyflows::model::{CURRENT_SCHEMA_VERSION, WorkflowGraph};

/// A legacy graph (no `schema_version`, no node `type_version`) migrates, loads
/// with defaults applied, compiles, and runs to completion.
#[tokio::test]
async fn legacy_graph_migrates_loads_and_runs() {
    // Legacy wire form: note the absent `schema_version` and the nodes without a
    // `type_version`. Edges also omit the port fields (they default to `main`).
    let legacy = json!({
        "name": "legacy_flow",
        "nodes": [
            { "id": "start", "kind": "trigger", "name": "Start" },
            {
                "id": "label",
                "kind": "transform",
                "name": "Label",
                "config": { "set": { "stage": "processed" } }
            },
            {
                "id": "call",
                "kind": "tool_call",
                "name": "Call",
                "config": { "slug": "slack.post", "args": { "channel": "ops" } }
            }
        ],
        "edges": [
            { "from_node": "start", "to_node": "label" },
            { "from_node": "label", "to_node": "call" }
        ]
    });

    // Sanity: the legacy value genuinely lacks the versioning fields.
    assert!(
        legacy.get("schema_version").is_none(),
        "the legacy fixture must not carry a schema_version"
    );
    assert!(
        legacy["nodes"][0].get("type_version").is_none(),
        "the legacy fixture must not carry a node type_version"
    );

    // Migrate on load, then deserialize into the typed model.
    let upgraded = migrate(legacy).expect("migrate");
    assert_eq!(
        upgraded.get("schema_version").and_then(|v| v.as_u64()),
        Some(u64::from(CURRENT_SCHEMA_VERSION)),
        "migrate should stamp the current schema_version onto the value"
    );

    let graph: WorkflowGraph = serde_json::from_value(upgraded).expect("deserialize");

    // The serde defaults filled in the missing versioning fields.
    assert_eq!(
        graph.schema_version, CURRENT_SCHEMA_VERSION,
        "the loaded graph should carry the current schema_version"
    );
    for node in &graph.nodes {
        assert_eq!(
            node.type_version, 1,
            "node `{}` should default to type_version 1",
            node.id
        );
    }
    // The port defaults were applied to the terse edges.
    assert!(
        graph
            .edges
            .iter()
            .all(|e| e.from_port == "main" && e.to_port == "main"),
        "terse edges should default both ports to `main`"
    );

    // The migrated graph compiles and runs to completion.
    let compiled = compile(&graph).expect("compile");
    let outcome = run(&compiled, json!({ "hello": "world" }), &mock_capabilities())
        .await
        .expect("run");

    // The trigger payload is preserved, and every node ran end to end.
    assert_eq!(
        outcome.output["run"]["trigger"],
        json!({ "hello": "world" })
    );
    assert_eq!(
        outcome.output["nodes"]["label"]["items"][0]["json"]["stage"],
        json!("processed"),
        "the transform node should have run and set its field"
    );
    assert_eq!(
        outcome.output["nodes"]["call"]["items"][0]["json"]["json"]["tool"],
        json!("slack.post"),
        "the tool_call node should have run and echoed its slug"
    );
    assert!(
        outcome.pending_approvals.is_empty(),
        "the run should complete with no pending approvals"
    );
}
