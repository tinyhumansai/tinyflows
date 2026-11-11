#![cfg(feature = "mock")]
//! Generated timing schedules for parallel branches.
//!
//! The same graph is run under opposite per-node latencies. Some branches fail
//! through `on_error: continue`; those error items are part of the deterministic
//! result too. Completion order must never leak into merged state.
//!
//! The second half runs generated graph shapes under two independent per-node
//! latency schedules with one shared failure set, injected through a
//! `StepInterceptor`: the stable output, or the error, must not depend on the
//! schedule.

mod support;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use proptest::prelude::*;
use serde_json::{Value, json};
use support::graphgen::{arb_shape, graph_of};
use tinyflows::caps::ToolInvoker;
use tinyflows::caps::mock::mock_capabilities;
use tinyflows::compiler::compile;
use tinyflows::engine::{CancellationToken, run, run_intercepted};
use tinyflows::error::EngineError;
use tinyflows::interception::{StepAction, StepFrame, StepInterceptor, StepPhase};
use tinyflows::model::{Edge, Node, NodeKind, WorkflowGraph};
use tinyflows::observability::{NoopObserver, RunObserver};

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

/// Per-node latency and failure injection, applied before each activation.
///
/// Latency is a count of `yield_now` calls, never wall-clock time: engine waits
/// use a global timer thread, so a millisecond schedule would not be
/// reproducible. A node fails only when it is in `failing` *and* carries
/// `on_error: continue`, so a same-named node inside a child workflow (whose
/// config was not rewritten) is never failed by accident.
struct Schedule {
    delays: Vec<u64>,
    failing: HashSet<String>,
}

#[async_trait]
impl StepInterceptor for Schedule {
    async fn intercept(&self, frame: StepFrame<'_>) -> StepAction {
        if frame.phase != StepPhase::Before {
            return StepAction::Continue { state_patch: None };
        }
        let id = frame.node.id.as_str();
        let delay = id
            .strip_prefix('n')
            .and_then(|index| index.parse::<usize>().ok())
            .map_or(0, |index| self.delays[index % self.delays.len()]);
        for _ in 0..delay {
            tokio::task::yield_now().await;
        }
        let handled = frame.node.config.get("on_error").and_then(Value::as_str) == Some("continue");
        if handled && self.failing.contains(id) {
            return StepAction::Fail {
                message: format!("scheduled failure at {id}"),
            };
        }
        StepAction::Continue { state_patch: None }
    }
}

/// Removes scheduler observations, as `fuzz_graph.rs` does, plus spawn
/// tickets: the task runner numbers them in start order, so they are opaque
/// handles that legitimately differ between latency schedules.
fn stable_output(mut output: Value) -> Value {
    fn strip(value: &mut Value) {
        match value {
            Value::Object(map) => {
                map.remove("_activation_step");
                map.remove("started_at_step");
                map.remove("polls");
                map.remove("ticket");
                map.values_mut().for_each(strip);
            }
            Value::Array(items) => items.iter_mut().for_each(strip),
            _ => {}
        }
    }
    strip(&mut output);
    output
}

/// Marks the leaf nodes selected by `failures` as `on_error: continue`, and
/// returns their ids. Only `output_parser` and `transform` leaves are marked:
/// a failure there is turned into an error item, so the run still completes
/// and its whole output, error items included, is compared.
fn mark_failures(graph: &mut WorkflowGraph, failures: u64) -> HashSet<String> {
    let mut failing = HashSet::new();
    for (index, node) in graph.nodes.iter_mut().enumerate() {
        let leaf = matches!(node.kind, NodeKind::OutputParser | NodeKind::Transform);
        if !leaf || failures & (1 << (index % 64)) == 0 {
            continue;
        }
        if !node.config.is_object() {
            node.config = json!({});
        }
        node.config["on_error"] = json!("continue");
        failing.insert(node.id.clone());
    }
    failing
}

fn run_scheduled(
    compiled: &tinyflows::compiler::CompiledWorkflow,
    delays: Vec<u64>,
    failing: HashSet<String>,
) -> Result<Value, String> {
    runtime().block_on(async {
        let observer = Arc::new(NoopObserver) as Arc<dyn RunObserver>;
        let schedule = Arc::new(Schedule { delays, failing });
        match tokio::time::timeout(
            Duration::from_secs(20),
            run_intercepted(
                compiled,
                json!({}),
                &mock_capabilities(),
                &observer,
                CancellationToken::new(),
                schedule,
            ),
        )
        .await
        {
            Err(_) => Err("HUNG: scheduled run did not terminate".to_string()),
            Ok(Ok((outcome, _resumable))) => Ok(stable_output(outcome.output)),
            Ok(Err(error)) => Err(error.to_string()),
        }
    })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, ..ProptestConfig::default() })]

    /// Two different latency schedules, one failure set: same stable output, or
    /// the same error.
    #[test]
    fn generated_shapes_are_deterministic_across_latency_schedules(
        shape in arb_shape(2),
        first in prop::collection::vec(0u64..10, 16),
        second in prop::collection::vec(0u64..10, 16),
        failures in any::<u64>(),
    ) {
        let mut graph = graph_of(&shape);
        let failing = mark_failures(&mut graph, failures);
        let Ok(compiled) = compile(&graph) else {
            return Ok(());
        };
        let a = run_scheduled(&compiled, first, failing.clone());
        let b = run_scheduled(&compiled, second, failing);
        prop_assert!(
            !matches!(&a, Err(e) if e.starts_with("HUNG")),
            "first schedule hung"
        );
        prop_assert_eq!(a, b, "latency schedule leaked into the result");
    }
}
