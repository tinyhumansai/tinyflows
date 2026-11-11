#![cfg(feature = "mock")]
//! Which error a run reports when several parallel branches fail together.
//!
//! The rule: within one superstep the failing branch with the lowest
//! active-set index names the run's error, whichever finished first. For a
//! static fan-out that index follows the fan-out node's edge order.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tinyflows::caps::mock::mock_capabilities;
use tinyflows::compiler::compile;
use tinyflows::engine::{CancellationToken, run_intercepted};
use tinyflows::interception::{StepAction, StepFrame, StepInterceptor, StepPhase};
use tinyflows::model::{Edge, Node, NodeKind, WorkflowGraph};
use tinyflows::observability::{NoopObserver, RunObserver};

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

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build runtime")
}

/// Fails `n1` and `n2` outright, after a per-node yield count.
struct FailBoth(Vec<u64>);

#[async_trait]
impl StepInterceptor for FailBoth {
    async fn intercept(&self, frame: StepFrame<'_>) -> StepAction {
        let index = match frame.node.id.as_str() {
            "n1" => 0,
            "n2" => 1,
            _ => return StepAction::Continue { state_patch: None },
        };
        if frame.phase != StepPhase::Before {
            return StepAction::Continue { state_patch: None };
        }
        for _ in 0..self.0[index] {
            tokio::task::yield_now().await;
        }
        StepAction::Fail {
            message: format!("unhandled failure at {}", frame.node.id),
        }
    }
}

/// Two parallel nodes fail without a handler, under opposite latencies. `n1`
/// is declared first, so it holds the lower active-set index and its error is
/// the one reported — also when `n2` finishes first.
#[test]
fn parallel_failures_report_the_lowest_ordered_branch_under_any_latency() {
    let graph = WorkflowGraph {
        name: "parallel_failures".to_string(),
        nodes: vec![
            node("trigger", NodeKind::Trigger, Value::Null),
            node("fan", NodeKind::OutputParser, Value::Null),
            node("n1", NodeKind::OutputParser, Value::Null),
            node("n2", NodeKind::OutputParser, Value::Null),
            node("join", NodeKind::Merge, Value::Null),
        ],
        edges: vec![
            edge("trigger", "fan"),
            edge("fan", "n1"),
            edge("fan", "n2"),
            edge("n1", "join"),
            edge("n2", "join"),
        ],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");

    let error_under = |delays: Vec<u64>| {
        runtime().block_on(async {
            let observer = Arc::new(NoopObserver) as Arc<dyn RunObserver>;
            run_intercepted(
                &compiled,
                json!({}),
                &mock_capabilities(),
                &observer,
                CancellationToken::new(),
                Arc::new(FailBoth(delays)),
            )
            .await
            .map(|_| ())
            .expect_err("both branches fail")
            .to_string()
        })
    };
    // `[10, 0]`: `n2` finishes first. `[0, 10]`: `n1` does.
    for delays in [vec![10, 0], vec![0, 10]] {
        let error = error_under(delays.clone());
        assert!(
            error.contains("unhandled failure at n1"),
            "delays {delays:?} reported {error:?}"
        );
    }
}
