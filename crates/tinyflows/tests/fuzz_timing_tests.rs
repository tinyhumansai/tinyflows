#![cfg(feature = "mock")]
//! Generated timing schedules for parallel branches.
//!
//! The same graph is run under opposite per-node latencies. Some branches fail
//! through `on_error: continue`; those error items are part of the deterministic
//! result too. Completion order must never leak into merged state.

use std::sync::Arc;
use std::time::Duration;

use proptest::prelude::*;
use serde_json::{Value, json};
use tinyflows::caps::ToolInvoker;
use tinyflows::caps::mock::mock_capabilities;
use tinyflows::compiler::compile;
use tinyflows::engine::run;
use tinyflows::error::EngineError;
use tinyflows::model::{Edge, Node, NodeKind, WorkflowGraph};

const GUARD: Duration = Duration::from_secs(5);

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

fn edge(from: &str, to: &str) -> Edge {
    Edge {
        from_node: from.to_string(),
        from_port: "main".to_string(),
        to_node: to.to_string(),
        to_port: "main".to_string(),
    }
}

fn graph(branches: usize, max_concurrency: usize) -> WorkflowGraph {
    let mut nodes = vec![
        node(
            "trigger",
            NodeKind::Trigger,
            json!({ "max_concurrency": max_concurrency }),
        ),
        node("fanout", NodeKind::OutputParser, Value::Null),
        node("merge", NodeKind::Merge, Value::Null),
    ];
    let mut edges = vec![edge("trigger", "fanout")];
    for index in 0..branches {
        let id = format!("branch_{index}");
        nodes.push(node(
            &id,
            NodeKind::ToolCall,
            json!({
                "slug": format!("branch.{index}"),
                "args": { "index": index },
                "on_error": "continue",
            }),
        ));
        edges.push(edge("fanout", &id));
        edges.push(edge(&id, "merge"));
    }
    WorkflowGraph {
        name: "generated_timing_schedule".to_string(),
        nodes,
        edges,
        ..Default::default()
    }
}

struct ScheduledTools {
    delays: Vec<u64>,
    failures: u16,
}

#[async_trait::async_trait]
impl ToolInvoker for ScheduledTools {
    async fn invoke(
        &self,
        slug: &str,
        args: Value,
        _conn: Option<&str>,
    ) -> tinyflows::error::Result<Value> {
        let index = slug
            .rsplit('.')
            .next()
            .and_then(|value| value.parse::<usize>().ok())
            .expect("generated branch slug");
        for _ in 0..self.delays.get(index).copied().unwrap_or(0) {
            tokio::task::yield_now().await;
        }
        if self.failures & (1 << index) != 0 {
            Err(EngineError::Capability(format!(
                "scheduled failure {index}"
            )))
        } else {
            Ok(json!({ "branch": index, "args": args }))
        }
    }
}

async fn run_schedule(
    compiled: &tinyflows::compiler::CompiledWorkflow,
    delays: Vec<u64>,
    failures: u16,
) -> Value {
    let mut caps = mock_capabilities();
    caps.tools = Arc::new(ScheduledTools { delays, failures });
    tokio::time::timeout(GUARD, run(compiled, json!({}), &caps))
        .await
        .expect("parallel run hung")
        .expect("on_error continue settles")
        .output
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build runtime")
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 96, ..ProptestConfig::default() })]

    #[test]
    fn merged_state_is_identical_under_opposite_completion_orders(
        branches in 2usize..9,
        raw_delays in prop::collection::vec(0u64..12, 8),
        failures in any::<u16>(),
        raw_limit in 1usize..9,
    ) {
        let delays: Vec<u64> = raw_delays.into_iter().take(branches).collect();
        let reverse: Vec<u64> = delays.iter().copied().rev().collect();
        let compiled = compile(&graph(branches, raw_limit.min(branches)))
            .expect("generated graph compiles");

        let (forward, backward) = runtime().block_on(async {
            tokio::join!(
                run_schedule(&compiled, delays, failures),
                run_schedule(&compiled, reverse, failures),
            )
        });
        prop_assert_eq!(forward, backward, "branch timing leaked into final state");
    }
}
