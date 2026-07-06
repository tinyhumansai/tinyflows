#![cfg(feature = "mock")]
//! Hardening probes for run lifecycle (audit BUG-6 failed-run observer, BUG-5
//! sub-workflow HITL dropped, BUG-7 cancel during retry backoff). Each test
//! asserts the correct behavior; failing ones are marked `#[ignore]`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tinyflows::caps::mock::mock_capabilities;
use tinyflows::compiler::compile;
use tinyflows::engine::{CancellationToken, run_cancellable, run_with_observer};
use tinyflows::model::{Edge, Node, NodeKind, WorkflowGraph};
use tinyflows::observability::{Run, RunObserver, RunStatus};

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

fn edge(from_node: &str, to_node: &str) -> Edge {
    Edge {
        from_node: from_node.to_string(),
        from_port: "main".to_string(),
        to_node: to_node.to_string(),
        to_port: "main".to_string(),
    }
}

/// Records whether the run lifecycle callbacks fired and the terminal status.
#[derive(Default)]
struct LifecycleObserver {
    started: AtomicBool,
    finished: AtomicBool,
    finished_completed: AtomicBool,
    steps: AtomicU64,
}

impl RunObserver for LifecycleObserver {
    fn on_run_start(&self, _run_id: &str) {
        self.started.store(true, Ordering::SeqCst);
    }
    fn on_step_finish(&self, _step: &tinyflows::observability::ExecutionStep) {
        self.steps.fetch_add(1, Ordering::SeqCst);
    }
    fn on_run_finish(&self, run: &Run) {
        self.finished.store(true, Ordering::SeqCst);
        self.finished_completed
            .store(run.status == RunStatus::Completed, Ordering::SeqCst);
    }
}

/// BUG-6 — on a `stop`-policy failure the run observer must still receive
/// `on_run_finish` (with a non-`Completed` status), not just `on_run_start`.
///
/// A `tool_call` with no `slug` fails; `on_error` defaults to `stop`. The
/// correct behavior surfaces a finished (Failed) run to the observer so a host
/// does not strand a "running forever" record.
#[tokio::test]
async fn bug6_failed_run_fires_on_run_finish() {
    let graph = WorkflowGraph {
        name: "bug6".to_string(),
        nodes: vec![
            trigger("start"),
            // No `slug` -> the tool_call node errors; default `on_error` = stop.
            node("bad", NodeKind::ToolCall, json!({ "args": { "x": 1 } })),
        ],
        edges: vec![edge("start", "bad")],
        ..Default::default()
    };

    let compiled = compile(&graph).expect("compile");
    // Keep a concrete handle to read the flags; hand a coerced trait-object
    // clone (same allocation) to the engine.
    let concrete = Arc::new(LifecycleObserver::default());
    let observer: Arc<dyn RunObserver> = concrete.clone();

    let result = run_with_observer(
        &compiled,
        json!({ "x": 1 }),
        &mock_capabilities(),
        &observer,
    )
    .await;
    assert!(
        result.is_err(),
        "a stop-policy tool_call failure should error the run"
    );

    assert!(
        concrete.started.load(Ordering::SeqCst),
        "on_run_start must fire"
    );
    assert!(
        concrete.finished.load(Ordering::SeqCst),
        "BUG-6: on_run_finish must ALSO fire for a failed run (observed: it never fires)"
    );
    assert!(
        !concrete.finished_completed.load(Ordering::SeqCst),
        "a failed run's terminal status must not be Completed"
    );
}

/// BUG-5 — a HITL approval gate inside a `sub_workflow` must be surfaced to the
/// parent, not silently swallowed. The parent must report the pending approval
/// (and must NOT complete as if the gated node had run).
#[tokio::test]
#[ignore = "BUG-5: sub_workflow keeps only outcome.output and discards the child's pending_approvals; the parent completes with empty pending_approvals, so a child HITL gate is unenforceable across the boundary"]
async fn bug5_sub_workflow_surfaces_child_pending_approval() {
    let child = WorkflowGraph {
        name: "child".to_string(),
        nodes: vec![
            trigger("c_start"),
            node(
                "gate",
                NodeKind::ToolCall,
                json!({ "slug": "pay", "requires_approval": true }),
            ),
            node("after", NodeKind::OutputParser, Value::Null),
        ],
        edges: vec![edge("c_start", "gate"), edge("gate", "after")],
        ..Default::default()
    };
    let child_value = serde_json::to_value(&child).expect("serialize child");

    let parent = WorkflowGraph {
        name: "parent".to_string(),
        nodes: vec![
            trigger("start"),
            node(
                "sub",
                NodeKind::SubWorkflow,
                json!({ "workflow": child_value }),
            ),
        ],
        edges: vec![edge("start", "sub")],
        ..Default::default()
    };

    let compiled = compile(&parent).expect("compile parent");
    let outcome = run_cancellable(
        &compiled,
        json!({ "amount": 100 }),
        &mock_capabilities(),
        CancellationToken::new(),
    )
    .await
    .expect("run parent");

    assert!(
        !outcome.pending_approvals.is_empty(),
        "BUG-5: parent must surface the child's pending approval gate (observed: parent completes \
         with empty pending_approvals, dropping the child gate)"
    );
}

/// BUG-7 — cancelling during a retry backoff must stop the run promptly, well
/// before the full `max_attempts * backoff_ms` budget elapses.
///
/// `bad` always fails (missing `slug`) with `retry:{max_attempts:5,
/// backoff_ms:400}` and `on_error:"continue"` (so the run settles rather than
/// erroring). Full budget ~= 4 * 400ms = 1600ms. We cancel ~200ms in; the
/// correct behavior is that the run winds down in well under a second.
#[tokio::test]
async fn bug7_cancel_during_retry_backoff_stops_promptly() {
    let graph = WorkflowGraph {
        name: "bug7".to_string(),
        nodes: vec![
            trigger("start"),
            node(
                "bad",
                NodeKind::ToolCall,
                json!({
                    "args": { "x": 1 },
                    "on_error": "continue",
                    "retry": { "max_attempts": 5, "backoff_ms": 400 }
                }),
            ),
        ],
        edges: vec![edge("start", "bad")],
        ..Default::default()
    };

    let compiled = compile(&graph).expect("compile");
    let caps = mock_capabilities();
    let token = CancellationToken::new();
    let token2 = token.clone();

    let start = Instant::now();
    let run_fut = run_cancellable(&compiled, json!({ "x": 1 }), &caps, token);
    let cancel_fut = async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        token2.cancel();
    };

    // Hard ceiling so the test can never hang regardless of the bug.
    let (run_res, _) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(run_fut, cancel_fut)
    })
    .await
    .expect("run must not hang past the 5s ceiling");

    let elapsed = start.elapsed();
    let outcome = run_res.expect("run");

    assert!(
        outcome.cancelled,
        "the run observed a cancelled token and should report cancelled"
    );
    assert!(
        elapsed < Duration::from_millis(1000),
        "BUG-7: cancel during backoff should stop the run promptly (<1s), but it took {elapsed:?} \
         (the retry loop keeps sleeping through the whole ~1600ms budget)"
    );
}
