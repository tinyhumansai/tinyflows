
#[tokio::test]
async fn error_item_has_node_and_message_fields() {
    // Assert the concrete shape of the emitted error item: json.error carries a
    // `node` (the failing node id) and a non-empty `message`.
    let mut tool = node("x", NodeKind::ToolCall);
    tool.config = json!({ "on_error": "continue" });
    let graph = WorkflowGraph {
        nodes: vec![node("t", NodeKind::Trigger), tool],
        edges: vec![edge("t", "x")],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let caps = mock_capabilities();

    let outcome = run(&compiled, json!({}), &caps).await.expect("run");
    let err = &outcome.output["nodes"]["x"]["items"][0]["json"]["error"];
    assert_eq!(err["node"], json!("x"));
    assert!(
        err["message"].as_str().is_some_and(|m| !m.is_empty()),
        "error item must carry a non-empty message, got {err:?}"
    );
}

/// Captures the terminal [`Run`] so a test can assert its status and which
/// nodes it names as failed (#661 L1).
#[derive(Default)]
struct StatusCapture {
    status: Mutex<Option<RunStatus>>,
    failed: Mutex<Vec<String>>,
}

impl RunObserver for StatusCapture {
    fn on_run_finish(&self, run: &Run) {
        *self.status.lock().unwrap() = Some(run.status.clone());
        *self.failed.lock().unwrap() = run
            .failed_node_ids()
            .iter()
            .map(|id| id.to_string())
            .collect();
    }
}

async fn observed_status(graph: &WorkflowGraph) -> (RunStatus, Vec<String>) {
    let compiled = compile(graph).expect("compile");
    let caps = mock_capabilities();
    let capture = Arc::new(StatusCapture::default());
    let observer: Arc<dyn RunObserver> = capture.clone();
    run_with_observer(&compiled, json!({}), &caps, &observer)
        .await
        .expect("run");
    let status = capture
        .status
        .lock()
        .unwrap()
        .clone()
        .expect("run finished");
    let failed = capture.failed.lock().unwrap().clone();
    (status, failed)
}

#[tokio::test]
async fn a_clean_run_is_completed_and_names_no_failed_node() {
    let graph = WorkflowGraph {
        nodes: vec![
            node("t", NodeKind::Trigger),
            node("a", NodeKind::OutputParser),
        ],
        edges: vec![edge("t", "a")],
        ..Default::default()
    };
    let (status, failed) = observed_status(&graph).await;
    assert_eq!(status, RunStatus::Completed);
    assert!(
        failed.is_empty(),
        "a clean run names no failed node: {failed:?}"
    );
}

#[tokio::test]
async fn on_error_continue_marks_the_run_completed_with_errors() {
    // #661 L1: a node that fails under `continue` used to leave the run
    // reporting an unqualified `Completed`, so a host read success while a
    // node failed. Now the terminal status says so, and names the node.
    let mut tool = node("x", NodeKind::ToolCall);
    tool.config = json!({ "on_error": "continue" });
    let graph = WorkflowGraph {
        nodes: vec![node("t", NodeKind::Trigger), tool],
        edges: vec![edge("t", "x")],
        ..Default::default()
    };
    let (status, failed) = observed_status(&graph).await;
    assert_eq!(status, RunStatus::CompletedWithErrors);
    assert_eq!(failed, vec!["x".to_string()], "the failing node is named");
}

#[tokio::test]
async fn on_error_route_marks_the_run_completed_with_errors() {
    // A routed failure also failed the node; the recovery branch handling it
    // downstream does not erase that the node itself errored.
    let mut tool = node("x", NodeKind::ToolCall);
    tool.config = json!({ "on_error": "route" });
    let graph = WorkflowGraph {
        nodes: vec![
            node("t", NodeKind::Trigger),
            tool,
            node("recover", NodeKind::OutputParser),
        ],
        edges: vec![edge("t", "x"), port_edge("x", "error", "recover")],
        ..Default::default()
    };
    let (status, failed) = observed_status(&graph).await;
    assert_eq!(status, RunStatus::CompletedWithErrors);
    assert!(
        failed.contains(&"x".to_string()),
        "the routed failure names its node: {failed:?}"
    );
}

#[tokio::test]
async fn retry_max_attempts_then_continue_completes() {
    // `retry.max_attempts` retries the failing node; after they are exhausted,
    // `on_error: continue` yields the error item and the run completes.
    let mut tool = node("x", NodeKind::ToolCall);
    tool.config = json!({ "retry": { "max_attempts": 4 }, "on_error": "continue" });
    let graph = WorkflowGraph {
        nodes: vec![node("t", NodeKind::Trigger), tool],
        edges: vec![edge("t", "x")],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let caps = mock_capabilities();

    let outcome = run(&compiled, json!({}), &caps).await.expect("run");
    let err = &outcome.output["nodes"]["x"]["items"][0]["json"]["error"];
    assert_eq!(err["node"], json!("x"));
    assert!(err["message"].as_str().is_some_and(|m| !m.is_empty()));
}

#[tokio::test]
async fn hitl_gate_pauses_and_blocks_downstream() {
    // A requires_approval gate with no approval pauses the run: reported pending
    // and its downstream never runs.
    let graph = WorkflowGraph {
        nodes: vec![
            node("t", NodeKind::Trigger),
            gate("g"),
            node("downstream", NodeKind::OutputParser),
        ],
        edges: vec![edge("t", "g"), edge("g", "downstream")],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let caps = mock_capabilities();

    let outcome = run(&compiled, json!({ "x": 1 }), &caps).await.expect("run");
    assert!(
        outcome.pending_approvals.contains(&"g".to_string()),
        "gate should be reported pending"
    );
    assert!(
        outcome.output["nodes"]["downstream"].is_null(),
        "downstream must not run behind a pending gate"
    );
}

#[tokio::test]
async fn hitl_two_gates_resume_one_leaves_next_pending() {
    // Two sequential gates: g1 -> g2 -> done. Resuming g1 lets g2 become the new
    // pending gate (done still blocked); a second resume of g2 completes the run.
    let graph = WorkflowGraph {
        nodes: vec![
            node("t", NodeKind::Trigger),
            gate("g1"),
            gate("g2"),
            node("done", NodeKind::OutputParser),
        ],
        edges: vec![edge("t", "g1"), edge("g1", "g2"), edge("g2", "done")],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let caps = mock_capabilities();

    let rr = run_resumable(&compiled, json!({}), &caps)
        .await
        .expect("run_resumable");
    assert!(
        rr.outcome().pending_approvals.contains(&"g1".to_string()),
        "g1 should be the first pending gate"
    );
    assert!(
        !rr.outcome().pending_approvals.contains(&"g2".to_string()),
        "g2 is not reached until g1 is approved"
    );

    let after_g1 = rr.resume(vec!["g1".to_string()]).await.expect("resume g1");
    assert!(
        after_g1.pending_approvals.contains(&"g2".to_string()),
        "g2 becomes pending after g1 is approved, got {:?}",
        after_g1.pending_approvals
    );
    assert!(
        after_g1.output["nodes"]["done"].is_null(),
        "done stays blocked while g2 is pending"
    );

    let done = rr.resume(vec!["g2".to_string()]).await.expect("resume g2");
    assert!(
        done.pending_approvals.is_empty(),
        "no gate pending once both are approved, got {:?}",
        done.pending_approvals
    );
    assert!(
        !done.output["nodes"]["done"]["items"].is_null(),
        "done runs once both gates are approved"
    );
}

#[tokio::test]
async fn approval_via_input_proceeds_immediately() {
    // Listing the gate id in the run input's `approvals` lets it proceed on the
    // first run with no pause: nothing pending, gate and downstream both run.
    let graph = WorkflowGraph {
        nodes: vec![
            node("t", NodeKind::Trigger),
            gate("g"),
            node("downstream", NodeKind::OutputParser),
        ],
        edges: vec![edge("t", "g"), edge("g", "downstream")],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let caps = mock_capabilities();

    let outcome = run(&compiled, json!({ "approvals": ["g"] }), &caps)
        .await
        .expect("run");
    assert!(
        outcome.pending_approvals.is_empty(),
        "an input-approved gate leaves nothing pending"
    );
    assert!(
        !outcome.output["nodes"]["g"]["items"].is_null(),
        "the approved gate itself runs"
    );
    assert!(
        !outcome.output["nodes"]["downstream"]["items"].is_null(),
        "downstream runs once the gate is approved via input"
    );
}

#[tokio::test]
async fn resume_replaces_non_object_input_with_approvals_object() {
    // The public rerun-based resume path accepts any JSON input. A scalar input
    // cannot preserve fields, so the engine replaces it with the approvals
    // object and the gate proceeds immediately.
    let graph = WorkflowGraph {
        nodes: vec![
            node("t", NodeKind::Trigger),
            gate("gate"),
            node("downstream", NodeKind::OutputParser),
        ],
        edges: vec![edge("t", "gate"), edge("gate", "downstream")],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let caps = mock_capabilities();

    let outcome = resume(
        &compiled,
        json!("raw-input"),
        vec!["gate".to_string()],
        &caps,
    )
    .await
    .expect("resume");

    assert!(outcome.pending_approvals.is_empty());
    assert_eq!(
        outcome.output["run"]["trigger"],
        json!({ "approvals": ["gate"] })
    );
    assert!(
        !outcome.output["nodes"]["downstream"]["items"].is_null(),
        "downstream should run after the scalar input is replaced with approvals"
    );
}

/// A [`RunObserver`] that counts run-start / run-finish and records step ids,
/// so a test can assert every hook fired the right number of times.
#[derive(Default)]
struct FullCapture {
    starts: Mutex<u32>,
    finishes: Mutex<u32>,
    steps: Mutex<Vec<String>>,
}

impl RunObserver for FullCapture {
    fn on_run_start(&self, _run_id: &str) {
        *self.starts.lock().unwrap() += 1;
    }

    fn on_step_finish(&self, step: &ExecutionStep) {
        self.steps.lock().unwrap().push(step.node_id.clone());
    }

    fn on_run_finish(&self, _run: &Run) {
        *self.finishes.lock().unwrap() += 1;
    }
}

#[tokio::test]
async fn observer_fires_start_finish_and_run_finish_counts() {
    // trigger -> a -> b. on_run_start fires once, on_run_finish once, and
    // on_step_finish fires once per non-trigger node (a, b) — never the trigger.
    let graph = WorkflowGraph {
        nodes: vec![
            node("t", NodeKind::Trigger),
            node("a", NodeKind::OutputParser),
            node("b", NodeKind::OutputParser),
        ],
        edges: vec![edge("t", "a"), edge("a", "b")],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let caps = mock_capabilities();

    let capture = Arc::new(FullCapture::default());
    let observer: Arc<dyn RunObserver> = capture.clone();
    run_with_observer(&compiled, json!({ "x": 1 }), &caps, &observer)
        .await
        .expect("run");

    assert_eq!(*capture.starts.lock().unwrap(), 1, "on_run_start once");
    assert_eq!(*capture.finishes.lock().unwrap(), 1, "on_run_finish once");
    let steps = capture.steps.lock().unwrap();
    assert_eq!(steps.len(), 2, "one step per non-trigger node");
    assert!(steps.contains(&"a".to_string()));
    assert!(steps.contains(&"b".to_string()));
    assert!(
        !steps.contains(&"t".to_string()),
        "the trigger must not produce a step"
    );
}

#[tokio::test]
async fn run_level_knobs_do_not_break_execution() {
    // A trigger carrying run-level recursion_limit + node_timeout_secs drives a
    // multi-node chain to completion, proving the knobs are wired without harm.
    let mut trigger = node("t", NodeKind::Trigger);
    trigger.config = json!({ "recursion_limit": 100, "node_timeout_secs": 30 });
    let graph = WorkflowGraph {
        nodes: vec![
            trigger,
            node("a", NodeKind::OutputParser),
            node("b", NodeKind::OutputParser),
        ],
        edges: vec![edge("t", "a"), edge("a", "b")],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let caps = mock_capabilities();

    let outcome = run(&compiled, json!({ "x": 1 }), &caps).await.expect("run");
    assert_eq!(
        outcome.output["nodes"]["b"]["items"][0]["json"],
        json!({ "x": 1 })
    );
}

#[tokio::test]
async fn trigger_only_completes_cleanly() {
    // A lone trigger runs to completion with nothing pending and its payload
    // seeded as the trigger node's single item.
    let graph = WorkflowGraph {
        nodes: vec![node("t", NodeKind::Trigger)],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let caps = mock_capabilities();

    let outcome = run(&compiled, json!({ "seed": 7 }), &caps)
        .await
        .expect("run");
    assert!(
        outcome.pending_approvals.is_empty(),
        "a trigger-only run has nothing pending"
    );
    assert_eq!(
        outcome.output["nodes"]["t"]["items"][0]["json"],
        json!({ "seed": 7 })
    );
}

// ---- Host-injectable checkpointer -------------------------------------

/// Compile-time proof that the handles a host holds across the gap between
/// run and resume are thread-safe: [`ResumableRun`] (kept alive across a
/// HITL pause) and [`RunOutcome`] (returned from every entry point) must be
/// `Send + Sync` so a host can move them between tasks/threads.
#[test]
fn resumable_run_and_outcome_are_send_sync() {
    fn _assert<T: Send + Sync>() {}
    _assert::<ResumableRun>();
    _assert::<RunOutcome>();
}
