
#[tokio::test]
async fn plain_run_and_resumable_unchanged_by_injectable_checkpointer() {
    // Regression: the default (non-injectable) `run` and `run_resumable`
    // paths must behave exactly as before. `run` drives a linear passthrough
    // to completion; `run_resumable` pauses at a gate and resumes from its
    // own in-memory checkpoint.
    let linear = WorkflowGraph {
        nodes: vec![
            node("t", NodeKind::Trigger),
            node("p", NodeKind::OutputParser),
        ],
        edges: vec![edge("t", "p")],
        ..Default::default()
    };
    let compiled = compile(&linear).expect("compile");
    let caps = mock_capabilities();
    let outcome = run(&compiled, json!({ "x": 1 }), &caps).await.expect("run");
    assert_eq!(
        outcome.output["nodes"]["p"]["items"][0]["json"],
        json!({ "x": 1 })
    );
    assert!(outcome.pending_approvals.is_empty());

    let mut gate = node("gate", NodeKind::OutputParser);
    gate.config = json!({ "requires_approval": true });
    let gated = WorkflowGraph {
        nodes: vec![
            node("t", NodeKind::Trigger),
            gate,
            node("downstream", NodeKind::OutputParser),
        ],
        edges: vec![edge("t", "gate"), edge("gate", "downstream")],
        ..Default::default()
    };
    let compiled = compile(&gated).expect("compile");
    let caps = mock_capabilities();
    let rr = run_resumable(&compiled, json!({}), &caps)
        .await
        .expect("run_resumable");
    assert!(rr.outcome().pending_approvals.contains(&"gate".to_string()));
    assert!(rr.outcome().output["nodes"]["downstream"].is_null());
    let done = rr.resume(vec!["gate".to_string()]).await.expect("resume");
    assert!(done.pending_approvals.is_empty());
    assert!(!done.output["nodes"]["downstream"]["items"].is_null());
}

#[tokio::test]
async fn uncancelled_token_runs_to_completion() {
    // A fresh (never-cancelled) token behaves exactly like `run`.
    let graph = WorkflowGraph {
        nodes: vec![
            node("t", NodeKind::Trigger),
            node("p", NodeKind::OutputParser),
        ],
        edges: vec![edge("t", "p")],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let token = CancellationToken::new();
    let outcome = run_cancellable(&compiled, json!({ "n": 1 }), &mock_capabilities(), token)
        .await
        .expect("run");
    assert!(!outcome.cancelled);
    assert_eq!(outcome.output["nodes"]["p"]["items"][0]["json"]["n"], 1);
}

#[tokio::test]
async fn cancelled_token_stops_run_and_reports_cancelled() {
    // trigger -> bad (a tool_call with no `slug`, on_error defaulting to
    // `stop`). If `bad` ever executed it would fail the whole run. Cancelling
    // the token before the run means `bad` short-circuits at its node
    // boundary instead of executing, so the run completes cleanly and reports
    // cancelled — proving new node work is not scheduled after cancellation.
    let graph = WorkflowGraph {
        nodes: vec![
            node("t", NodeKind::Trigger),
            node("bad", NodeKind::ToolCall),
        ],
        edges: vec![edge("t", "bad")],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let token = CancellationToken::new();
    token.cancel();
    let outcome = run_cancellable(&compiled, json!({ "n": 1 }), &mock_capabilities(), token)
        .await
        .expect("cancelled run still returns Ok");
    assert!(outcome.cancelled, "outcome should report cancelled");
    // `bad` short-circuited: it emitted an empty item list, not a tool result
    // and not a run-ending error.
    let items = &outcome.output["nodes"]["bad"]["items"];
    assert!(
        items.as_array().is_some_and(|a| a.is_empty()),
        "cancelled node should emit no items, got: {items:?}"
    );
}

#[test]
fn cancellation_token_flips_and_is_shared_across_clones() {
    let token = CancellationToken::new();
    let clone = token.clone();
    assert!(!token.is_cancelled());
    assert!(!clone.is_cancelled());
    clone.cancel();
    // Both handles observe the flip — they share one atomic flag.
    assert!(token.is_cancelled());
    assert!(clone.is_cancelled());
}
