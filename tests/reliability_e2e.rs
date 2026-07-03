#![cfg(feature = "mock")]
//! Reliability end-to-end tests that exercise real engine execution paths using
//! *custom* host-capability implementations swapped into the otherwise-mock
//! [`Capabilities`](tinyflows::caps::Capabilities) bundle.
//!
//! Unlike `tests/reference_workflows.rs`, which drives the deterministic mock
//! capabilities, these tests replace exactly one capability slot with a bespoke
//! `Arc<dyn ...>` impl to observe engine behavior that the mocks can't provoke:
//!
//! 1. the retry loop recovering after transient failures (`FlakyTool`),
//! 2. a per-node timeout firing on a slow capability (`SlowCode`), and
//! 3. the recursion limit bounding a genuinely cyclic graph.
//!
//! Gated behind the `mock` cargo feature so `cargo test` (no features) skips it
//! while `cargo test --all-features` runs it. Every scenario uses tiny
//! timeouts/limits so the suite can never hang.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

use tinyflows::caps::mock::mock_capabilities;
use tinyflows::caps::{CodeLanguage, CodeRunner, ToolInvoker};
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

/// A [`ToolInvoker`] whose first two invocations fail and every invocation after
/// that succeeds — used to prove the engine's retry loop recovers on a later
/// attempt (the "first `Ok` breaks the loop" branch).
struct FlakyTool {
    /// Number of times [`FlakyTool::invoke`] has been entered.
    calls: AtomicUsize,
}

#[async_trait]
impl ToolInvoker for FlakyTool {
    async fn invoke(&self, _slug: &str, _args: Value, _conn: Option<&str>) -> CapResult<Value> {
        // 0-based index of this attempt. Attempts 0 and 1 fail; attempt 2+ succeed.
        let attempt = self.calls.fetch_add(1, Ordering::SeqCst);
        if attempt < 2 {
            Err(EngineError::Capability(format!(
                "flaky transient failure on attempt {}",
                attempt + 1
            )))
        } else {
            Ok(json!({ "ok": true }))
        }
    }
}

#[tokio::test]
async fn retry_recovers_after_transient_failures() {
    // trigger -> call(tool_call){ retry: max_attempts=3 }. The FlakyTool fails the
    // first two invocations and succeeds on the third; the retry loop must use that
    // success, so the node's output item is the SUCCESS payload (not an error item).
    let flaky = Arc::new(FlakyTool {
        calls: AtomicUsize::new(0),
    });
    let mut caps = mock_capabilities();
    caps.tools = flaky.clone();

    let graph = WorkflowGraph {
        name: "retry_recovers".to_string(),
        nodes: vec![
            node("t", NodeKind::Trigger, Value::Null),
            node(
                "call",
                NodeKind::ToolCall,
                json!({ "slug": "x", "retry": { "max_attempts": 3 } }),
            ),
        ],
        edges: vec![edge("t", "call")],
        ..Default::default()
    };

    let compiled = compile(&graph).expect("compile");
    let outcome = run(&compiled, json!({}), &caps)
        .await
        .expect("run should recover once the tool succeeds on attempt 3");

    let item = &outcome.output["nodes"]["call"]["items"][0]["json"];
    assert_eq!(
        item,
        &json!({ "ok": true }),
        "the recovered success payload should be the node's output item"
    );
    assert!(
        item.get("error").is_none(),
        "a recovered node must not emit an error item, got: {item}"
    );
    assert_eq!(
        flaky.calls.load(Ordering::SeqCst),
        3,
        "the tool should be invoked exactly three times (fail, fail, succeed)"
    );
}

/// A [`CodeRunner`] that sleeps well past a one-second node timeout before it
/// would return — used to prove a per-node timeout aborts the node.
struct SlowCode;

#[async_trait]
impl CodeRunner for SlowCode {
    async fn run(&self, _language: CodeLanguage, _source: &str, _input: Value) -> CapResult<Value> {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        Ok(json!({ "done": true }))
    }
}

#[tokio::test]
async fn node_timeout_fires_on_a_slow_capability() {
    // trigger{node_timeout_secs=1} -> slow(code). The SlowCode node awaits 1.5s,
    // exceeding the 1s per-node timeout, so the run must error rather than complete.
    let mut caps = mock_capabilities();
    caps.code = Arc::new(SlowCode);

    let graph = WorkflowGraph {
        name: "node_timeout".to_string(),
        nodes: vec![
            node("t", NodeKind::Trigger, json!({ "node_timeout_secs": 1 })),
            node("slow", NodeKind::Code, json!({})),
        ],
        edges: vec![edge("t", "slow")],
        ..Default::default()
    };

    let compiled = compile(&graph).expect("compile");
    let result = run(&compiled, json!({}), &caps).await;

    assert!(
        result.is_err(),
        "the node should exceed its 1s timeout and fail the run, got: {result:?}"
    );
}

#[tokio::test]
async fn recursion_limit_bounds_a_cycle() {
    // A genuinely cyclic graph: trigger -> a -> b -> trigger (the b->t edge closes
    // the loop). With a small recursion_limit the run must terminate with an error
    // instead of looping forever. A 10s outer guard ensures the test itself can
    // never hang the suite even if the limit were not enforced.
    //
    // The back-edge targets the trigger rather than `a` on purpose: a loop closing
    // on `a` (trigger -> a -> b -> a) gives `a` two predecessors, so the engine
    // lowers its incoming edges as a fan-in merge barrier (waiting edges). That
    // barrier deadlocks the loop before it can iterate — the run then settles as
    // `Ok` with only the trigger having run, and the recursion limit is never
    // exercised. Closing the loop on the (single-predecessor) trigger keeps every
    // edge a plain edge, so the graph actually loops and the limit fires.
    // The cycle is pure control flow (output_parser passthroughs), so the stock
    // mock capabilities are fine — no slot needs swapping here.
    let caps = mock_capabilities();

    let graph = WorkflowGraph {
        name: "recursion_cycle".to_string(),
        nodes: vec![
            node("t", NodeKind::Trigger, json!({ "recursion_limit": 5 })),
            node("a", NodeKind::OutputParser, Value::Null),
            node("b", NodeKind::OutputParser, Value::Null),
        ],
        edges: vec![edge("t", "a"), edge("a", "b"), edge("b", "t")],
        ..Default::default()
    };

    let compiled = compile(&graph).expect("compile");
    let guarded = tokio::time::timeout(Duration::from_secs(10), run(&compiled, json!({}), &caps));
    match guarded.await {
        Err(_elapsed) => panic!("run hung past 10s — the recursion limit did not bound the cycle"),
        Ok(inner) => assert!(
            inner.is_err(),
            "the recursion/step limit should error the cyclic run, got: {inner:?}"
        ),
    }
}
