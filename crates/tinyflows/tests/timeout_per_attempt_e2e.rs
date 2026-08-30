#![cfg(feature = "mock")]
//! End-to-end test for BUG-8: `node_timeout_secs` bounds EACH retry attempt,
//! not the whole retry loop.
//!
//! A node with `node_timeout_secs: 1` and `retry.max_attempts: 2` must give
//! *each* attempt its own ~1s budget: the two attempts run back-to-back for
//! ~2s total, and the underlying (slow) capability is entered once per attempt.
//! Under the old graph-level `with_node_timeout` behavior the whole node was
//! killed at 1s mid-first-attempt, so the capability would have been entered
//! only once and the run would end after ~1s — this test distinguishes the two.
//!
//! Gated behind the `mock` cargo feature (like `tests/reliability_e2e.rs`), so
//! `cargo test` (no features) skips it and `cargo test --all-features` runs it.
//! An outer `tokio::time::timeout` guards against any hang.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{Value, json};

use tinyflows::caps::ToolInvoker;
use tinyflows::caps::mock::mock_capabilities;
use tinyflows::compiler::compile;
use tinyflows::engine::run;
use tinyflows::error::{EngineError, Result as CapResult};
use tinyflows::model::{Edge, Node, NodeKind, WorkflowGraph};

/// Builds a node with the given id, kind, and config (no ports, no position).
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

/// Builds a `main` -> `main` edge from `from` to `to`.
fn edge(from: &str, to: &str) -> Edge {
    Edge {
        from_node: from.to_string(),
        from_port: "main".to_string(),
        to_node: to.to_string(),
        to_port: "main".to_string(),
    }
}

/// A [`ToolInvoker`] that records each entry then sleeps far longer than the
/// per-attempt timeout — so every attempt times out rather than completing. The
/// entry counter proves how many attempts were actually *started* (each attempt
/// increments it exactly once, before the sleep), which is the signal that the
/// timeout is applied per attempt rather than to the whole retry loop.
struct SlowTool {
    /// Number of times [`SlowTool::invoke`] has been entered (one per attempt).
    calls: AtomicUsize,
}

#[async_trait]
impl ToolInvoker for SlowTool {
    async fn invoke(&self, _slug: &str, _args: Value, _conn: Option<&str>) -> CapResult<Value> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        // Sleep well past the 1s per-attempt timeout; the attempt is cancelled
        // (dropped) at ~1s long before this resolves, so the returned value is
        // never actually observed.
        tokio::time::sleep(Duration::from_secs(5)).await;
        Err(EngineError::Capability(
            "slow tool never completes".to_string(),
        ))
    }
}

#[tokio::test]
async fn node_timeout_is_bounded_per_attempt() {
    // trigger{node_timeout_secs=1} -> call(tool_call){ retry: max_attempts=2 }.
    // Each of the 2 attempts is bounded to ~1s, so:
    //   * the SlowTool is entered exactly twice (once per attempt), and
    //   * the whole node takes ~2s (2 x ~1s), not ~1s (whole-loop kill) and not
    //     ~5s (a single un-bounded attempt running the full sleep).
    let slow = Arc::new(SlowTool {
        calls: AtomicUsize::new(0),
    });
    let mut caps = mock_capabilities();
    caps.tools = slow.clone();

    let graph = WorkflowGraph {
        name: "timeout_per_attempt".to_string(),
        nodes: vec![
            node("t", NodeKind::Trigger, json!({ "node_timeout_secs": 1 })),
            node(
                "call",
                NodeKind::ToolCall,
                json!({ "slug": "x", "retry": { "max_attempts": 2 } }),
            ),
        ],
        edges: vec![edge("t", "call")],
        ..Default::default()
    };

    let compiled = compile(&graph).expect("compile");

    let started = Instant::now();
    // Outer guard so the test can never hang the suite even if the per-attempt
    // bound regressed and an attempt ran the full 5s sleep for both attempts.
    let guarded = tokio::time::timeout(Duration::from_secs(12), run(&compiled, json!({}), &caps));
    let result = match guarded.await {
        Err(_elapsed) => panic!("run hung past 12s — per-attempt timeout did not fire"),
        Ok(inner) => inner,
    };
    let elapsed = started.elapsed();

    // Both attempts timed out; default `on_error` is "stop", so the run fails.
    assert!(
        result.is_err(),
        "both attempts should time out and (on_error=stop) fail the run, got: {result:?}"
    );

    // The core assertion: the tool was entered once per attempt. A whole-loop
    // timeout would have killed the node during the first attempt (1 call).
    assert_eq!(
        slow.calls.load(Ordering::SeqCst),
        2,
        "the slow tool must be entered once per attempt (2), proving each attempt is bounded"
    );

    // Timing corroborates per-attempt bounding: ~2s total (2 x ~1s). The lower
    // bound rules out a whole-loop 1s kill (~1s); the upper bound rules out any
    // attempt running the full 5s sleep.
    assert!(
        elapsed >= Duration::from_millis(1800),
        "two ~1s attempts should take >= ~1.8s total, got {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(4),
        "each attempt is bounded to ~1s so the node should finish well under 4s, got {elapsed:?}"
    );
}
