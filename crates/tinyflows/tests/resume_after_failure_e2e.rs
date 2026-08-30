#![cfg(feature = "mock")]
//! Continuing a run that *failed*, rather than one that paused.
//!
//! The engine has always been able to answer a pause with a decision. This is
//! the other boundary: a node broke, the runtime committed everything before
//! it, and the question is whether the caller can pick the run back up instead
//! of starting it over.
//!
//! Every test here counts **invocations**, not state. A state diff cannot tell
//! a prefix that was skipped from a prefix that ran again and produced the same
//! thing — and for a prefix that posts a comment or opens a pull request, those
//! are opposite outcomes. `fuzz_resume.rs` says exactly this in its own module
//! doc and leaves the counter "with the work that fixes it"; this is that work.

mod support;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value, json};
use tinyflows::caps::mock::mock_capabilities;
use tinyflows::caps::{Capabilities, ToolInvoker};
use tinyflows::compiler::compile;
use tinyflows::engine::{
    InMemoryCheckpointer, RunInput, failure_boundary, retry_with_checkpointer,
    run_with_checkpointer,
};
use tinyflows::error::{EngineError, Result as EngineResult};
use tinyflows::model::{Edge, Node, NodeKind, WorkflowGraph};

/// A tool host that counts every call and can be told to break one slug.
///
/// The counter is the whole point: it is the only thing that distinguishes
/// "the prefix was reused" from "the prefix ran again and looked the same".
#[derive(Default)]
struct CountingTools {
    calls: Mutex<HashMap<String, usize>>,
    broken: Mutex<Option<String>>,
}

impl CountingTools {
    fn with_broken(slug: &str) -> Arc<Self> {
        let tools = Arc::new(Self::default());
        *tools.broken.lock().expect("lock") = Some(slug.to_string());
        tools
    }

    /// How many times `slug` was invoked across every run so far.
    fn calls(&self, slug: &str) -> usize {
        self.calls
            .lock()
            .expect("lock")
            .get(slug)
            .copied()
            .unwrap_or(0)
    }

    /// Stop breaking whatever was broken — the "someone fixed it" step.
    fn repair(&self) {
        *self.broken.lock().expect("lock") = None;
    }
}

#[async_trait]
impl ToolInvoker for CountingTools {
    async fn invoke(&self, slug: &str, _args: Value, _conn: Option<&str>) -> EngineResult<Value> {
        *self
            .calls
            .lock()
            .expect("lock")
            .entry(slug.to_string())
            .or_insert(0) += 1;
        if self.broken.lock().expect("lock").as_deref() == Some(slug) {
            return Err(EngineError::Capability(format!("{slug} is down")));
        }
        Ok(json!({ "slug": slug, "ok": true }))
    }
}

fn caps(tools: Arc<CountingTools>) -> Capabilities {
    Capabilities {
        tools,
        ..mock_capabilities()
    }
}

/// `start → post_comment → tally → summarise`, all tool calls.
///
/// Named for effects on purpose. The middle step is the one broken in these
/// tests, so `post_comment` is the step a re-run from the trigger would
/// perform twice — which is the cost this feature exists to avoid.
fn graph() -> WorkflowGraph {
    let tool = |id: &str| Node {
        id: id.to_string(),
        kind: NodeKind::ToolCall,
        type_version: 1,
        name: id.to_string(),
        config: json!({ "slug": id, "args": {} }),
        ports: Vec::new(),
        position: None,
    };
    let edge = |from: &str, to: &str| Edge {
        from_node: from.to_string(),
        from_port: "main".to_string(),
        to_node: to.to_string(),
        to_port: "main".to_string(),
    };
    WorkflowGraph {
        schema_version: 1,
        id: None,
        name: "review and report".to_string(),
        inputs: Vec::new(),
        agents: Vec::new(),
        nodes: vec![
            Node {
                id: "start".to_string(),
                kind: NodeKind::Trigger,
                type_version: 1,
                name: "start".to_string(),
                config: json!({ "trigger_kind": "manual" }),
                ports: Vec::new(),
                position: None,
            },
            tool("post_comment"),
            tool("tally"),
            tool("summarise"),
        ],
        edges: vec![
            edge("start", "post_comment"),
            edge("post_comment", "tally"),
            edge("tally", "summarise"),
        ],
    }
}

#[tokio::test]
async fn a_continued_run_finishes_the_tail_without_repeating_the_prefix() {
    // The property the whole feature rests on, and the one a state diff cannot
    // see: the effectful prefix happens exactly once across a failure and the
    // continue that follows it.
    let tools = CountingTools::with_broken("tally");
    let compiled = compile(&graph()).expect("compiles");
    let checkpointer = Arc::new(InMemoryCheckpointer::new());
    let thread = "run-1";

    let failed = run_with_checkpointer(
        &compiled,
        RunInput::new(json!({})),
        &caps(tools.clone()),
        checkpointer.clone(),
        thread,
    )
    .await;
    assert!(failed.is_err(), "the broken tool must fail the run");
    assert_eq!(tools.calls("post_comment"), 1, "the prefix ran once");
    assert_eq!(tools.calls("summarise"), 0, "the tail did not run at all");

    let boundary = failure_boundary(&(checkpointer.clone() as _), thread)
        .await
        .expect("readable")
        .expect("a failed run leaves a boundary to continue from");
    assert_eq!(boundary.failed_node, "tally");
    assert!(boundary.error.contains("tally is down"), "{boundary:?}");
    assert_eq!(
        boundary.pending,
        vec!["tally".to_string()],
        "the failed node is what a continue would run"
    );

    // Someone fixes the thing that was broken, and the run picks up where it
    // stopped.
    tools.repair();
    let outcome = retry_with_checkpointer(&compiled, &caps(tools.clone()), checkpointer, thread)
        .await
        .expect("the continued run completes");

    assert_eq!(
        tools.calls("post_comment"),
        1,
        "THE POINT: the effectful prefix was not performed a second time"
    );
    assert_eq!(
        tools.calls("tally"),
        2,
        "the failed node ran again — once broken, once fixed"
    );
    assert_eq!(tools.calls("summarise"), 1, "and the tail finally ran");
    assert!(
        outcome.pending_approvals.is_empty(),
        "nothing was waiting on a human"
    );
    // The prefix's output survived in committed state rather than being
    // recomputed — the run's final state carries all three steps.
    for node in ["post_comment", "tally", "summarise"] {
        assert!(
            outcome.output["nodes"].get(node).is_some(),
            "{node} missing from the continued run's state: {}",
            outcome.output["nodes"]
        );
    }
}

#[tokio::test]
async fn a_run_that_is_still_broken_fails_again_and_leaves_the_boundary_standing() {
    // A continue is not a fix. Retrying without repairing must not consume the
    // boundary — an operator who tries twice before finding the cause has to
    // still be able to continue afterwards.
    let tools = CountingTools::with_broken("tally");
    let compiled = compile(&graph()).expect("compiles");
    let checkpointer = Arc::new(InMemoryCheckpointer::new());
    let thread = "run-2";

    let _ = run_with_checkpointer(
        &compiled,
        RunInput::new(json!({})),
        &caps(tools.clone()),
        checkpointer.clone(),
        thread,
    )
    .await;

    let again = retry_with_checkpointer(
        &compiled,
        &caps(tools.clone()),
        checkpointer.clone(),
        thread,
    )
    .await;
    assert!(again.is_err(), "still broken, so still failing");
    assert_eq!(
        tools.calls("post_comment"),
        1,
        "and still without repeating the prefix"
    );

    let boundary = failure_boundary(&(checkpointer as _), thread)
        .await
        .expect("readable")
        .expect("the boundary survives a failed continue");
    assert_eq!(boundary.failed_node, "tally");
}

#[tokio::test]
async fn a_completed_run_reports_no_failure_boundary() {
    // The negative half. `failure_boundary` is read after an `Err`, but a
    // caller that reads it anywhere else must not be told a healthy thread has
    // something to continue.
    let tools = Arc::new(CountingTools::default());
    let compiled = compile(&graph()).expect("compiles");
    let checkpointer = Arc::new(InMemoryCheckpointer::new());
    let thread = "run-3";

    run_with_checkpointer(
        &compiled,
        RunInput::new(json!({})),
        &caps(tools.clone()),
        checkpointer.clone(),
        thread,
    )
    .await
    .expect("nothing is broken");

    assert!(
        failure_boundary(&(checkpointer as _), thread)
            .await
            .expect("readable")
            .is_none(),
        "a run that finished has no failure to continue from"
    );
}

#[tokio::test]
async fn a_thread_nothing_ever_ran_on_has_no_boundary() {
    let checkpointer: Arc<dyn tinyflows::engine::Checkpointer<Value>> =
        Arc::new(InMemoryCheckpointer::new());
    assert!(
        failure_boundary(&checkpointer, "never-ran")
            .await
            .expect("readable")
            .is_none()
    );
}
