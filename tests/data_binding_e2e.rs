#![cfg(feature = "mock")]
//! End-to-end test for **inline data-binding** into integration-node config.
//!
//! Capability-backed nodes (`agent`, `tool_call`, `http_request`) resolve any
//! `=`-expressions embedded in their config against the node's input before the
//! config reaches the host capability (see `src/expr.rs`'s `resolve` and
//! `src/nodes/mod.rs`'s `expr_scope`, which builds `{ item, items, run }`).
//!
//! Here a manual trigger feeds `{ name, channel }` into a `tool_call` whose
//! `args` reference `=item.channel` and `=item.name`. The mock `ToolInvoker`
//! echoes the args it was invoked with, so the run output proves the upstream
//! trigger data flowed into the resolved tool arguments end-to-end.
//!
//! Gated behind the `mock` feature, so `cargo test` (no features) skips this file
//! while `cargo test --all-features` runs it.

use serde_json::json;
use tinyflows::caps::mock::mock_capabilities;
use tinyflows::compiler::compile;
use tinyflows::engine::run;
use tinyflows::model::{Edge, Node, NodeKind, TriggerKind, WorkflowGraph};

#[tokio::test]
async fn tool_call_args_bind_to_upstream_trigger_data() {
    let graph = WorkflowGraph {
        name: "data_binding".to_string(),
        nodes: vec![
            Node {
                id: "trigger".into(),
                kind: NodeKind::Trigger,
                type_version: 1,
                name: "trigger".into(),
                config: json!({ "kind": TriggerKind::Manual }),
                ports: vec![],
                position: None,
            },
            Node {
                id: "post".into(),
                kind: NodeKind::ToolCall,
                type_version: 1,
                name: "post".into(),
                config: json!({
                    "slug": "slack.send",
                    "args": { "channel": "=item.channel", "text": "=item.name" },
                }),
                ports: vec![],
                position: None,
            },
        ],
        edges: vec![Edge {
            from_node: "trigger".into(),
            from_port: "main".into(),
            to_node: "post".into(),
            to_port: "main".into(),
        }],
        ..Default::default()
    };

    let compiled = compile(&graph).expect("compile");
    let outcome = run(
        &compiled,
        json!({ "name": "Ada", "channel": "#ops" }),
        &mock_capabilities(),
    )
    .await
    .expect("run should complete");

    // The `=item.*` expressions resolved against the trigger payload before the
    // mock tool echoed the args back — proving inline data-binding end-to-end.
    assert_eq!(
        outcome.output["nodes"]["post"]["items"][0]["json"]["args"],
        json!({ "channel": "#ops", "text": "Ada" })
    );
}
