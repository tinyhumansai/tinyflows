use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio::time::sleep;

use crate::caps::mock::mock_capabilities;
use crate::caps::{Capabilities, ToolInvoker};
use crate::compiler::compile;
use crate::engine::{CancellationToken, RunOutcome, run_cancellable};
use crate::error::Result;
use crate::model::{Edge, Node, NodeKind, WorkflowGraph};

/// The bounded spin `slow` uses to wait for cancellation, and the cap on
/// `marker`'s sleep — both generous enough to never trip under load, small
/// enough that a broken build (where `marker` runs) is still obvious.
const SPIN_CAP_MS: u64 = 5_000;

/// A [`ToolInvoker`] that records every slug it runs and lets a test suspend a
/// run *inside* a node. `slow` fires `slow_started` and then — when
/// `block_until_cancel` is set — holds the run at that node until `run_token`
/// flips (bounded by [`SPIN_CAP_MS`]); this pins the cancel to land while
/// `slow` is mid-flight, with no dependency on scheduler timing. `marker` is a
/// node that must never run once cancellation has propagated: its appearance
/// in `invoked` (and its long sleep in the elapsed time) is what a broken
/// propagation reveals.
#[derive(Clone)]
struct ProbeTools {
    invoked: Arc<Mutex<Vec<String>>>,
    slow_started: Arc<Notify>,
    run_token: CancellationToken,
    block_until_cancel: bool,
    marker_ms: u64,
}

#[async_trait]
impl ToolInvoker for ProbeTools {
    async fn invoke(&self, slug: &str, _args: Value, _conn: Option<&str>) -> Result<Value> {
        self.invoked
            .lock()
            .expect("invoked mutex")
            .push(slug.to_string());
        match slug {
            "slow" => {
                self.slow_started.notify_one();
                // Hold the child at this node until the run is cancelled, so
                // the boundary check before `marker` deterministically sees
                // the flip. Bounded so a broken build cannot hang CI.
                if self.block_until_cancel {
                    let mut waited = 0;
                    while !self.run_token.is_cancelled() && waited < SPIN_CAP_MS {
                        sleep(Duration::from_millis(1)).await;
                        waited += 1;
                    }
                }
            }
            // When cancellation propagated, `marker` never runs. A cancel test
            // sets `marker_ms` long so a *broken* propagation (marker runs)
            // blows the elapsed bound too; the uncancelled control sets it to 0
            // so the run stays fast when `marker` legitimately runs.
            "marker" => sleep(Duration::from_millis(self.marker_ms)).await,
            _ => {}
        }
        Ok(json!({ "tool": slug }))
    }
}

fn node(id: &str, kind: NodeKind) -> Node {
    Node {
        id: id.to_string(),
        kind,
        type_version: 1,
        name: id.to_string(),
        config: Value::Null,
        ports: Vec::new(),
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

/// A single-invocation `tool_call` node bound to `slug`.
fn tool(id: &str, slug: &str) -> Node {
    let mut n = node(id, NodeKind::ToolCall);
    n.config = json!({ "slug": slug, "execution": "once" });
    n
}

/// `trigger -> slow -> marker`: the innermost chain every test cancels within.
fn slow_then_marker() -> WorkflowGraph {
    WorkflowGraph {
        nodes: vec![
            node("ct", NodeKind::Trigger),
            tool("slow", "slow"),
            tool("marker", "marker"),
        ],
        edges: vec![edge("ct", "slow"), edge("slow", "marker")],
        ..Default::default()
    }
}

/// `trigger -> sub_workflow(child)`, with `child` embedded inline.
fn wrap_inline(child: &WorkflowGraph) -> WorkflowGraph {
    let inline = serde_json::to_value(child).expect("serialize child graph");
    let mut sw = node("sw", NodeKind::SubWorkflow);
    sw.config = json!({ "workflow": inline, "execution": "once" });
    WorkflowGraph {
        nodes: vec![node("pt", NodeKind::Trigger), sw],
        edges: vec![edge("pt", "sw")],
        ..Default::default()
    }
}

fn probe(
    token: &CancellationToken,
    block_until_cancel: bool,
    marker_ms: u64,
) -> (Capabilities, Arc<Mutex<Vec<String>>>, Arc<Notify>) {
    let invoked = Arc::new(Mutex::new(Vec::new()));
    let slow_started = Arc::new(Notify::new());
    let tools = ProbeTools {
        invoked: invoked.clone(),
        slow_started: slow_started.clone(),
        run_token: token.clone(),
        block_until_cancel,
        marker_ms,
    };
    let caps = Capabilities {
        tools: Arc::new(tools),
        ..mock_capabilities()
    };
    (caps, invoked, slow_started)
}

/// Drives `run_cancellable` to completion while **actively polling** the run
/// future, cancelling `token` the moment the innermost `slow` node starts.
/// Polling matters: an un-awaited run future is the documented trap that makes
/// a cancellation test hollow, so the future is raced against the start signal
/// rather than cancelled blind.
async fn run_cancelling_on_slow(
    graph: &WorkflowGraph,
    caps: &Capabilities,
    token: CancellationToken,
    slow_started: Arc<Notify>,
) -> (RunOutcome, Duration) {
    let compiled = compile(graph).expect("compile");
    let fut = run_cancellable(&compiled, json!({}), caps, token.clone());
    tokio::pin!(fut);
    let notified = slow_started.notified();
    tokio::pin!(notified);
    let mut cancelled = false;
    let started = Instant::now();
    let outcome = loop {
        tokio::select! {
            out = &mut fut => break out.expect("cancelled run still returns Ok"),
            // Guarded so the one-shot start signal is not polled after it fires.
            () = &mut notified, if !cancelled => {
                token.cancel();
                cancelled = true;
            }
        }
    };
    assert!(
        cancelled,
        "the `slow` node must have started so the cancel landed mid-flight"
    );
    (outcome, started.elapsed())
}

// T1 — repro→green. Parent -> sub_workflow(child), child is
// trigger -> slow -> marker. Cancelling mid-`slow` must wind the child down:
// `slow` (already running) completes, `marker` never runs, and the run
// settles cancelled. Before the fix the child ran behind a fresh token, so
// the cancel never crossed the boundary and `marker` executed.
#[tokio::test]
async fn t1_parent_cancel_stops_child_before_marker() {
    let token = CancellationToken::new();
    let (caps, invoked, slow_started) = probe(&token, true, SPIN_CAP_MS);
    let graph = wrap_inline(&slow_then_marker());

    let (outcome, elapsed) = run_cancelling_on_slow(&graph, &caps, token, slow_started).await;

    assert!(outcome.cancelled, "the parent run should report cancelled");
    let slugs = invoked.lock().expect("invoked mutex").clone();
    assert!(
        slugs.contains(&"slow".to_string()),
        "the in-flight `slow` node ran: {slugs:?}"
    );
    assert!(
        !slugs.contains(&"marker".to_string()),
        "cancellation must reach the child: `marker` should never run, got {slugs:?}"
    );
    // Elapsed is bounded by the in-flight `slow` node's remainder, not by
    // `marker`'s long sleep — the run did not wait on the skipped node.
    assert!(
        elapsed < Duration::from_millis(2_000),
        "wind-down should be bounded by `slow`, not `marker`; took {elapsed:?}"
    );
}

// T2 — transitive inheritance at depth ≥ 2. Parent -> sub -> sub, the
// innermost being trigger -> slow -> marker. Cancelling mid-innermost-`slow`
// must still skip the innermost `marker`: each level forwards the same token
// down through its own node contexts.
#[tokio::test]
async fn t2_cancel_propagates_through_two_levels() {
    let token = CancellationToken::new();
    let (caps, invoked, slow_started) = probe(&token, true, SPIN_CAP_MS);
    let inner = wrap_inline(&slow_then_marker());
    let graph = wrap_inline(&inner);

    let (outcome, elapsed) = run_cancelling_on_slow(&graph, &caps, token, slow_started).await;

    assert!(outcome.cancelled, "the top run should report cancelled");
    let slugs = invoked.lock().expect("invoked mutex").clone();
    assert!(
        slugs.contains(&"slow".to_string()),
        "innermost `slow` ran: {slugs:?}"
    );
    assert!(
        !slugs.contains(&"marker".to_string()),
        "the token must reach depth 2: innermost `marker` should never run, got {slugs:?}"
    );
    assert!(
        elapsed < Duration::from_millis(2_000),
        "two-level wind-down should still be bounded by `slow`; took {elapsed:?}"
    );
}

// T3 — the guardrail: an *uncancelled* run of the identical graph must be
// untouched, so the fix cannot be "cancel everything". Both child slugs run
// and the outcome is not cancelled. `block_until_cancel` is off so `slow`
// returns immediately (nothing will ever cancel it).
#[tokio::test]
async fn t3_uncancelled_child_runs_to_completion() {
    let token = CancellationToken::new();
    let (caps, invoked, _slow_started) = probe(&token, false, 0);
    let graph = wrap_inline(&slow_then_marker());
    let compiled = compile(&graph).expect("compile");

    let outcome = run_cancellable(&compiled, json!({}), &caps, token)
        .await
        .expect("run");

    assert!(
        !outcome.cancelled,
        "an uncancelled run must not report cancelled"
    );
    let slugs = invoked.lock().expect("invoked mutex").clone();
    assert!(
        slugs.contains(&"slow".to_string()),
        "`slow` should run: {slugs:?}"
    );
    assert!(
        slugs.contains(&"marker".to_string()),
        "`marker` must still run when nothing cancels: {slugs:?}"
    );
}

// T4 — a token already cancelled before the run starts: the parent's
// `sub_workflow` node short-circuits at its own boundary, so the child never
// starts and neither tool is invoked.
#[tokio::test]
async fn t4_pre_cancelled_token_never_starts_child() {
    let token = CancellationToken::new();
    let (caps, invoked, _slow_started) = probe(&token, true, 0);
    let graph = wrap_inline(&slow_then_marker());
    let compiled = compile(&graph).expect("compile");

    token.cancel();
    let outcome = run_cancellable(&compiled, json!({}), &caps, token)
        .await
        .expect("pre-cancelled run still returns Ok");

    assert!(outcome.cancelled, "a pre-cancelled run reports cancelled");
    let slugs = invoked.lock().expect("invoked mutex").clone();
    assert!(
        slugs.is_empty(),
        "the sub_workflow node short-circuits, so no child tool runs: {slugs:?}"
    );
}

// T5 — the defensive arm. When a child reports cancelled but the parent's own
// token is *not* set, `run_child` still errors rather than silently treating
// the child as completed. That state is unreachable through `run_sub_workflow`
// today (a child only ever receives the parent's own token clone, so a
// cancelled child implies a cancelled parent token — see T1), so this pins the
// arm at the source level: a future refactor cannot delete it and let an
// independently-cancelled child fall through as a false completion.
#[test]
fn t5_defensive_independent_cancel_arm_is_present() {
    // `run_child` lives in the production-only execution module, separate from
    // this test file, so assertion text cannot accidentally satisfy the check.
    let production = include_str!("sub_workflow/execution.rs");
    assert!(
        production.contains("if ctx.token.is_cancelled() {")
            && production.contains("run is halted rather than falsely completed"),
        "run_child must keep BOTH the parent-cancel wind-down (Ok(None)) and the \
             defensive independent-cancel error arm"
    );
}
