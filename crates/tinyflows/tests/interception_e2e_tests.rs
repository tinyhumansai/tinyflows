#![cfg(feature = "mock")]
//! End-to-end tests for the [`StepInterceptor`] execution-gating hook.
//!
//! [`RunObserver`] can watch a run and never change one; a [`StepInterceptor`]
//! is obeyed. These tests cover both halves of that claim:
//!
//! - the **no-cost property** — an attached-but-inert interceptor leaves a run
//!   byte-identical, in outcome *and* in recorded steps, to one with no
//!   interceptor at all. Everything else here is only safe because of this;
//! - that each [`StepAction`] does what it says, landing the activation back on
//!   the engine's existing paths (routing, `on_error`, the observer's records)
//!   rather than inventing control flow beside them.
//!
//! Gated behind the `mock` feature, so plain `cargo test` skips it while
//! `cargo test --all-features` runs it.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value, json};
use tinyflows::caps::mock::mock_capabilities;
use tinyflows::compiler::compile;
use tinyflows::data::Item;
use tinyflows::engine::{CancellationToken, run_intercepted, run_with_observer};
use tinyflows::interception::{StepAction, StepFrame, StepInterceptor, StepPhase};
use tinyflows::model::{Edge, Node, NodeKind, TriggerKind, WorkflowGraph};
use tinyflows::observability::{ExecutionStep, NoopObserver, RunObserver};

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
    node(
        id,
        NodeKind::Trigger,
        json!({ "kind": TriggerKind::Manual }),
    )
}

fn edge(from_node: &str, to_node: &str) -> Edge {
    Edge {
        from_node: from_node.to_string(),
        from_port: "main".to_string(),
        to_node: to_node.to_string(),
        to_port: "main".to_string(),
    }
}

/// A trigger feeding a `tool_call` feeding a `transform` that echoes what it
/// received, so a downstream assertion can see whatever the tool node emitted.
fn graph() -> WorkflowGraph {
    WorkflowGraph {
        name: "intercepted".to_string(),
        nodes: vec![
            trigger("t"),
            node("call", NodeKind::ToolCall, json!({ "slug": "svc.do" })),
            node(
                "after",
                NodeKind::Transform,
                json!({ "set": { "seen": "=item" } }),
            ),
        ],
        edges: vec![edge("t", "call"), edge("call", "after")],
        ..Default::default()
    }
}

/// Records every step the engine reports, for comparing two runs.
#[derive(Default)]
struct Steps(Mutex<Vec<(String, String, Value)>>);

impl RunObserver for Steps {
    fn on_step_finish(&self, step: &ExecutionStep) {
        let status = format!("{:?}", step.status);
        self.0.lock().expect("steps lock").push((
            step.node_id.clone(),
            status,
            step.output.clone(),
        ));
    }
}

impl Steps {
    fn taken(&self) -> Vec<(String, String, Value)> {
        self.0.lock().expect("steps lock").clone()
    }
}

/// An interceptor that answers `Continue` to everything — attached, and inert.
struct Inert;

#[async_trait]
impl StepInterceptor for Inert {
    async fn intercept(&self, _frame: StepFrame<'_>) -> StepAction {
        StepAction::Continue { state_patch: None }
    }
}

/// Applies one action at one phase to one node, and passes everything else
/// through untouched.
struct ActOnce {
    node_id: &'static str,
    phase: StepPhase,
    action: Mutex<Option<StepAction>>,
}

impl ActOnce {
    fn new(node_id: &'static str, phase: StepPhase, action: StepAction) -> Arc<Self> {
        Arc::new(Self {
            node_id,
            phase,
            action: Mutex::new(Some(action)),
        })
    }
}

#[async_trait]
impl StepInterceptor for ActOnce {
    async fn intercept(&self, frame: StepFrame<'_>) -> StepAction {
        if frame.phase == self.phase && frame.node.id == self.node_id {
            if let Some(action) = self.action.lock().expect("action lock").take() {
                return action;
            }
        }
        StepAction::Continue { state_patch: None }
    }
}

/// The property everything else depends on: attaching an inert interceptor
/// changes nothing a caller can observe.
///
/// Asserted over the outcome *and* the full step record, because those are the
/// two things a host reads. If this ever fails, the interception seam has
/// started costing correctness rather than only being available.
#[tokio::test]
async fn an_inert_interceptor_leaves_a_run_byte_identical() {
    let compiled = compile(&graph()).expect("compile");
    let caps = mock_capabilities();

    let plain_steps = Arc::new(Steps::default());
    let plain = run_with_observer(
        &compiled,
        json!({ "q": "go" }),
        &caps,
        &(plain_steps.clone() as Arc<dyn RunObserver>),
    )
    .await
    .expect("plain run");

    let hooked_steps = Arc::new(Steps::default());
    let (hooked, _resumable) = run_intercepted(
        &compiled,
        json!({ "q": "go" }),
        &caps,
        &(hooked_steps.clone() as Arc<dyn RunObserver>),
        CancellationToken::new(),
        Arc::new(Inert),
    )
    .await
    .expect("intercepted run");

    assert_eq!(
        plain.output, hooked.output,
        "an inert interceptor must not change the run state"
    );
    assert_eq!(plain.pending_approvals, hooked.pending_approvals);
    assert_eq!(plain.cancelled, hooked.cancelled);
    assert_eq!(
        plain_steps.taken(),
        hooked_steps.taken(),
        "an inert interceptor must not change what the observer records"
    );
}

/// `Replace` at `Before` stands in an answer without running the executor, and
/// the substituted value reaches the downstream node.
#[tokio::test]
async fn replacing_before_skips_the_executor_and_feeds_downstream() {
    let compiled = compile(&graph()).expect("compile");
    let hook = ActOnce::new(
        "call",
        StepPhase::Before,
        StepAction::Replace {
            items: vec![Item::new(json!({ "stubbed": true }))],
            port: None,
        },
    );

    let (outcome, _resumable) = run_intercepted(
        &compiled,
        json!({}),
        &mock_capabilities(),
        &(Arc::new(NoopObserver) as Arc<dyn RunObserver>),
        CancellationToken::new(),
        hook,
    )
    .await
    .expect("run");

    // The tool node emitted the substituted item rather than the mock's echo,
    // which would have carried a `tool` key.
    assert_eq!(
        outcome.output["nodes"]["call"]["items"][0]["json"],
        json!({ "stubbed": true })
    );
    // And the downstream transform saw it, so the substitution went through
    // ordinary routing rather than stopping at the node it replaced.
    assert_eq!(
        outcome.output["nodes"]["after"]["items"][0]["json"]["seen"],
        json!({ "stubbed": true })
    );
}

/// A substituted activation is still recorded as a step, so a replaced node
/// does not silently vanish from a run's history.
#[tokio::test]
async fn a_replaced_activation_is_still_reported_to_the_observer() {
    let compiled = compile(&graph()).expect("compile");
    let steps = Arc::new(Steps::default());
    let hook = ActOnce::new(
        "call",
        StepPhase::Before,
        StepAction::Replace {
            items: vec![Item::new(json!({ "stubbed": true }))],
            port: None,
        },
    );

    run_intercepted(
        &compiled,
        json!({}),
        &mock_capabilities(),
        &(steps.clone() as Arc<dyn RunObserver>),
        CancellationToken::new(),
        hook,
    )
    .await
    .expect("run");

    let recorded = steps.taken();
    let call = recorded
        .iter()
        .find(|(id, _, _)| id == "call")
        .expect("the replaced node still reports a step");
    assert_eq!(call.1, "Success");
    assert_eq!(
        call.2[0]["json"],
        json!({ "stubbed": true }),
        "the observer must see the substituted output, not the discarded one"
    );
}

/// `Fail` at `Before` enters the node's own `on_error` policy rather than
/// inventing a second failure path beside it.
#[tokio::test]
async fn injecting_a_failure_enters_the_nodes_on_error_policy() {
    let mut graph = graph();
    graph.nodes[1].config = json!({ "slug": "svc.do", "on_error": "continue" });
    let compiled = compile(&graph).expect("compile");
    let hook = ActOnce::new(
        "call",
        StepPhase::Before,
        StepAction::Fail {
            message: "injected outage".to_string(),
        },
    );

    let (outcome, _resumable) = run_intercepted(
        &compiled,
        json!({}),
        &mock_capabilities(),
        &(Arc::new(NoopObserver) as Arc<dyn RunObserver>),
        CancellationToken::new(),
        hook,
    )
    .await
    .expect("`on_error: continue` turns the failure into data, so the run completes");

    let error = &outcome.output["nodes"]["call"]["items"][0]["json"]["error"];
    assert!(
        error.to_string().contains("injected outage"),
        "the injected message should reach the error item, got {error}"
    );
}

/// `Replace` at `After` rescues a genuinely failed activation: the side effect
/// already happened, and only what the graph sees downstream changes.
#[tokio::test]
async fn replacing_after_rescues_a_failed_activation() {
    let mut graph = graph();
    // No `slug`, so the tool node deterministically fails.
    graph.nodes[1].config = json!({});
    let compiled = compile(&graph).expect("compile");

    // Without the interceptor this run fails outright — the default `on_error`
    // is `stop`. Establish that first, so the rescue below is meaningful.
    let unrescued = run_with_observer(
        &compiled,
        json!({}),
        &mock_capabilities(),
        &(Arc::new(NoopObserver) as Arc<dyn RunObserver>),
    )
    .await;
    assert!(
        unrescued.is_err(),
        "the unrescued run should fail, otherwise this test proves nothing"
    );

    let hook = ActOnce::new(
        "call",
        StepPhase::After,
        StepAction::Replace {
            items: vec![Item::new(json!({ "rescued": true }))],
            port: None,
        },
    );
    let (outcome, _resumable) = run_intercepted(
        &compiled,
        json!({}),
        &mock_capabilities(),
        &(Arc::new(NoopObserver) as Arc<dyn RunObserver>),
        CancellationToken::new(),
        hook,
    )
    .await
    .expect("the rescued run completes");

    assert_eq!(
        outcome.output["nodes"]["after"]["items"][0]["json"]["seen"],
        json!({ "rescued": true })
    );
}

/// A `Before` state patch changes what the node reads *and* survives the node
/// writing its own slot, so a later node sees the edit too.
#[tokio::test]
async fn a_before_state_patch_is_visible_downstream() {
    let graph = WorkflowGraph {
        name: "patched".to_string(),
        nodes: vec![
            trigger("t"),
            node(
                "read",
                NodeKind::Transform,
                json!({ "set": { "note": "=run.injected" } }),
            ),
        ],
        edges: vec![edge("t", "read")],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let hook = ActOnce::new(
        "read",
        StepPhase::Before,
        StepAction::Continue {
            state_patch: Some(json!({ "run": { "injected": "from the debugger" } })),
        },
    );

    let (outcome, _resumable) = run_intercepted(
        &compiled,
        json!({}),
        &mock_capabilities(),
        &(Arc::new(NoopObserver) as Arc<dyn RunObserver>),
        CancellationToken::new(),
        hook,
    )
    .await
    .expect("run");

    assert_eq!(
        outcome.output["nodes"]["read"]["items"][0]["json"]["note"],
        json!("from the debugger"),
        "the node should have read the patched state"
    );
    assert_eq!(
        outcome.output["run"]["injected"],
        json!("from the debugger"),
        "and the patch should have been committed, not just read"
    );
}

include!("interception_e2e/interception_part_01_tests.rs");
