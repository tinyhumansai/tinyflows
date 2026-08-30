#![cfg(feature = "mock")]
//! A human review that runs **alongside** the graph rather than stopping it.
//!
//! The suspending default is right when there is nothing else to get on with:
//! the run pauses, costs nothing, and the host resumes it. But a workflow often
//! has independent work that does not depend on the verdict — enrich the
//! record, fetch the related rows, precompute the thing you would publish — and
//! that work should not sit idle behind a person.
//!
//! Wiring the review as its own branch in `wait_mode: "poll"` gives exactly
//! that: the review re-activates on an interval while the sibling branch keeps
//! advancing through its own super-steps, the decision lands mid-run, and a
//! `merge` absorbs both. What this file proves is the ordering — the sibling
//! branch **finishes while the review is still outstanding**, which is the
//! difference between "concurrent" and merely "eventually".

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tinyflows::caps::mock::mock_capabilities;
use tinyflows::caps::{
    ApprovalDecision, ApprovalOutcome, ApprovalProvider, ApprovalRequest, Capabilities,
};
use tinyflows::compiler::compile;
use tinyflows::engine::{RunInput, run_with_observer};
use tinyflows::error::Result;
use tinyflows::model::{Edge, Node, NodeKind, WorkflowGraph};
use tinyflows::observability::{ExecutionStep, RunObserver, StepStatus};

const GUARD: Duration = Duration::from_secs(10);

/// A review desk whose human takes `answer_after` looks to respond, so the
/// review is genuinely outstanding while the rest of the graph runs.
struct SlowDesk {
    answer_after: usize,
    calls: AtomicUsize,
    seen: Mutex<Vec<String>>,
}

impl SlowDesk {
    fn new(answer_after: usize) -> Self {
        Self {
            answer_after,
            calls: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
        }
    }

    fn request_ids(&self) -> Vec<String> {
        self.seen.lock().expect("lock").clone()
    }
}

#[async_trait]
impl ApprovalProvider for SlowDesk {
    async fn decide(&self, request: &ApprovalRequest) -> Result<ApprovalOutcome> {
        self.seen
            .lock()
            .expect("lock")
            .push(request.request_id.clone());
        let seen = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if seen < self.answer_after {
            return Ok(ApprovalOutcome::Pending);
        }
        Ok(ApprovalOutcome::Decided(ApprovalDecision {
            approved: true,
            decided_by: Some("editor".into()),
            comment: Some("looks right".into()),
            payload: None,
        }))
    }
}

/// Records the order in which nodes finished, which is how this test tells
/// concurrency from mere eventual completion.
#[derive(Default)]
struct FinishOrder {
    order: Mutex<Vec<String>>,
}

impl FinishOrder {
    /// Where a node **first** appears. A polling node records a step per poll,
    /// so for `review` this is the first look, not the verdict.
    fn first_position_of(&self, node: &str) -> Option<usize> {
        self.order
            .lock()
            .expect("lock")
            .iter()
            .position(|id| id == node)
    }

    /// Where a node **last** appears — for a polling node, the activation that
    /// actually settled it.
    fn last_position_of(&self, node: &str) -> Option<usize> {
        self.order
            .lock()
            .expect("lock")
            .iter()
            .rposition(|id| id == node)
    }

    fn count_of(&self, node: &str) -> usize {
        self.order
            .lock()
            .expect("lock")
            .iter()
            .filter(|id| *id == node)
            .count()
    }

    fn finished(&self) -> Vec<String> {
        self.order.lock().expect("lock").clone()
    }
}

impl RunObserver for FinishOrder {
    fn on_step_finish(&self, step: &ExecutionStep) {
        if matches!(step.status, StepStatus::Success) {
            self.order.lock().expect("lock").push(step.node_id.clone());
        }
    }
}

fn node(id: &str, kind: NodeKind, config: Value) -> Node {
    Node {
        id: id.into(),
        kind,
        type_version: 1,
        name: id.into(),
        config,
        ports: vec![],
        position: None,
    }
}

fn edge(from: &str, port: &str, to: &str, to_port: &str) -> Edge {
    Edge {
        from_node: from.into(),
        from_port: port.into(),
        to_node: to.into(),
        to_port: to_port.into(),
    }
}

/// trigger fans out to two branches:
///
/// - `review` — an `approval` in poll mode, waiting on a human;
/// - `enrich -> score -> summarize` — independent work that must not wait.
///
/// Both land on `combine`, a `merge` barrier that absorbs the verdict and the
/// work together.
fn graph() -> WorkflowGraph {
    WorkflowGraph {
        name: "async approval".into(),
        nodes: vec![
            node("trigger", NodeKind::Trigger, Value::Null),
            node(
                "review",
                NodeKind::Approval,
                json!({
                    "title": "Publish this?",
                    "subject_kind": "url",
                    "subject": "=item.url",
                    "wait_mode": "poll",
                    "poll_interval_ms": 1,
                    "max_polls": 50,
                }),
            ),
            node(
                "enrich",
                NodeKind::Transform,
                json!({ "set": { "url": "=item.url", "enriched": true } }),
            ),
            node(
                "score",
                NodeKind::Transform,
                json!({ "set": { "url": "=item.url", "score": 91 } }),
            ),
            node(
                "summarize",
                NodeKind::Transform,
                json!({ "set": { "url": "=item.url", "score": "=item.score", "ready": true } }),
            ),
            node("combine", NodeKind::Merge, json!({ "mode": "append" })),
        ],
        edges: vec![
            edge("trigger", "main", "review", "main"),
            edge("trigger", "main", "enrich", "main"),
            edge("enrich", "main", "score", "main"),
            edge("score", "main", "summarize", "main"),
            edge("review", "approved", "combine", "verdict"),
            edge("summarize", "main", "combine", "work"),
        ],
        ..Default::default()
    }
}

#[tokio::test]
async fn a_polling_review_lets_the_rest_of_the_graph_run_and_then_absorbs_the_verdict() {
    let compiled = compile(&graph()).expect("compile");
    // The human answers on the 4th look — long enough that the three-node work
    // branch has somewhere to get to in the meantime.
    let desk = Arc::new(SlowDesk::new(4));
    let caps = Capabilities {
        approvals: Some(desk.clone()),
        ..mock_capabilities()
    };
    let order = Arc::new(FinishOrder::default());
    let observer: Arc<dyn RunObserver> = order.clone();

    let outcome = tokio::time::timeout(
        GUARD,
        run_with_observer(
            &compiled,
            RunInput::new(json!({ "url": "https://example.com/drafts/42" }))
                .with_run_id("run-async-1"),
            &caps,
            &observer,
        ),
    )
    .await
    .expect("the run must not hang waiting on the review")
    .expect("run");

    // 1. The run completed on its own. Nothing paused: a polling review never
    //    interrupts, so the host was never asked to resume anything.
    assert!(
        outcome.pending_approvals.is_empty(),
        "a polling review must not pause the run, got {:?}",
        outcome.pending_approvals
    );

    // 2. The work branch ran to completion WHILE the review was outstanding.
    //    The finish order interleaves — review, enrich, review, score, review,
    //    summarize, review — because each poll is its own activation and the
    //    sibling branch advances in the super-steps between them. So the test
    //    is: the review was still being polled *before* the work started, and
    //    it settled *after* the work had finished.
    let finished = order.finished();
    let first_review = order
        .first_position_of("review")
        .unwrap_or_else(|| panic!("the review must have been polled, saw {finished:?}"));
    let settled_review = order
        .last_position_of("review")
        .unwrap_or_else(|| panic!("the review must have settled, saw {finished:?}"));
    let enrich = order
        .first_position_of("enrich")
        .unwrap_or_else(|| panic!("the work branch must have started, saw {finished:?}"));
    let summarize = order
        .last_position_of("summarize")
        .unwrap_or_else(|| panic!("the work branch must have finished, saw {finished:?}"));

    // This ordering is not a scheduler race: this test runs on the default
    // `current_thread` tokio flavor (one poller, no cross-thread scheduling),
    // and neither `review`'s first poll (`SlowDesk::decide`, a synchronous
    // `std::sync::Mutex` push with no real I/O) nor `enrich` (a synchronous
    // jq transform) suspends on its first poll. `run_active_parallel`
    // (src/graph/compiled/executor/node_execution.rs) drives the two
    // trigger-fanned-out branches via `futures_util::future::join_all`, which
    // polls each branch's future in the order it was pushed — `review`'s edge
    // is declared before `enrich`'s in `graph()` above — so `review` runs to
    // completion, including firing `on_step_finish`, before `enrich` is ever
    // polled.
    assert!(
        first_review < enrich,
        "the review must already be outstanding when the work branch starts — \
         finish order was {finished:?}"
    );
    assert!(
        summarize < settled_review,
        "the work branch must finish before the verdict lands, not after it — \
         finish order was {finished:?}"
    );
    assert!(
        order.count_of("review") > 1,
        "the review must have been polled repeatedly rather than settling at once — \
         finish order was {finished:?}"
    );

    // 3. The human was asked more than once, always about the SAME review.
    //    A polling loop that re-registered the request would notify a person
    //    once per poll; create-or-fetch on `request_id` is what prevents that.
    let ids = desk.request_ids();
    assert!(
        ids.len() >= 2,
        "the review should have been outstanding across several polls, got {ids:?}"
    );
    assert!(
        ids.iter().all(|id| id == "run-async-1:review"),
        "every poll must address one review, got {ids:?}"
    );

    // 4. The graph absorbed the verdict: the merge carries both the decision
    //    and the work the branch did while waiting for it.
    let merged = outcome.output["nodes"]["combine"]["items"]
        .as_array()
        .expect("the merge emitted items");
    let jsons: Vec<&Value> = merged.iter().map(|item| &item["json"]).collect();
    assert!(
        jsons.iter().any(|json| json["approved"] == json!(true)),
        "the verdict must reach the merge, got {jsons:?}"
    );
    assert!(
        jsons.iter().any(|json| json["ready"] == json!(true)),
        "the work done while waiting must reach the merge, got {jsons:?}"
    );
}

/// The same shape, but the human says no: the graph still ran its independent
/// work, and the rejection routes to its own branch rather than failing the run.
#[tokio::test]
async fn a_rejection_arriving_late_routes_without_disturbing_the_work_branch() {
    struct RejectingDesk;

    #[async_trait]
    impl ApprovalProvider for RejectingDesk {
        async fn decide(&self, _request: &ApprovalRequest) -> Result<ApprovalOutcome> {
            Ok(ApprovalOutcome::Decided(ApprovalDecision::rejected(Some(
                "wrong draft".into(),
            ))))
        }
    }

    let mut graph = graph();
    graph.nodes.push(node(
        "revise",
        NodeKind::Transform,
        json!({ "set": { "revise_because": "=item.comment" } }),
    ));
    graph
        .edges
        .push(edge("review", "rejected", "revise", "main"));

    let compiled = compile(&graph).expect("compile");
    let caps = Capabilities {
        approvals: Some(Arc::new(RejectingDesk)),
        ..mock_capabilities()
    };

    let outcome = tokio::time::timeout(
        GUARD,
        run_with_observer(
            &compiled,
            RunInput::new(json!({ "url": "https://example.com/drafts/42" }))
                .with_run_id("run-async-2"),
            &caps,
            &(Arc::new(tinyflows::observability::NoopObserver) as Arc<dyn RunObserver>),
        ),
    )
    .await
    .expect("the run must not hang")
    .expect("run");

    assert_eq!(outcome.output["nodes"]["review"]["port"], "rejected");
    assert_eq!(
        outcome.output["nodes"]["revise"]["items"][0]["json"]["revise_because"], "wrong draft",
        "the rejection branch runs with the reviewer's reason"
    );
    assert_eq!(
        outcome.output["nodes"]["summarize"]["items"][0]["json"]["ready"],
        json!(true),
        "the independent work branch still completed"
    );
}
