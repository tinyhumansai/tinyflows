
#[tokio::test]
async fn durable_resume_via_injected_checkpointer() {
    // A SHARED, externally-held checkpointer simulates a host's durable store
    // that survives across "processes": we run under it, then rebuild caps +
    // graph and resume from it by thread id alone.
    let cp: Arc<dyn Checkpointer<Value>> = Arc::new(InMemoryCheckpointer::<Value>::default());

    // trigger -> gate{requires_approval} -> downstream(output_parser).
    let mut gate = node("gate", NodeKind::OutputParser);
    gate.config = json!({ "requires_approval": true });
    let graph = WorkflowGraph {
        nodes: vec![
            node("t", NodeKind::Trigger),
            gate,
            node("downstream", NodeKind::OutputParser),
        ],
        edges: vec![edge("t", "gate"), edge("gate", "downstream")],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");

    // "Process 1": run under the host checkpointer, pausing at the gate.
    let caps = mock_capabilities();
    let paused = run_with_checkpointer(&compiled, json!({}), &caps, cp.clone(), "thread-A")
        .await
        .expect("run_with_checkpointer");
    assert_eq!(
        paused.pending_approvals,
        vec!["gate".to_string()],
        "the gate must be reported pending"
    );
    assert!(
        paused.output["nodes"]["downstream"].is_null(),
        "downstream must not run behind a pending gate"
    );

    // "Process 2": fresh caps, same durable checkpointer + thread id.
    let caps = mock_capabilities();
    let done = resume_with_checkpointer(
        &compiled,
        &caps,
        cp.clone(),
        "thread-A",
        vec!["gate".to_string()],
    )
    .await
    .expect("resume_with_checkpointer");
    assert!(
        done.pending_approvals.is_empty(),
        "nothing should be pending once the gate is approved, got {:?}",
        done.pending_approvals
    );
    assert!(
        !done.output["nodes"]["downstream"]["items"].is_null(),
        "downstream should run once the run resumes from the durable checkpoint"
    );
}

#[tokio::test]
async fn resume_denying_a_gate_routes_to_its_error_port() {
    // trigger -> gate{requires_approval}; gate has BOTH a `main` edge (to
    // `downstream`) and an `error` edge (to `recover`). Denying the gate on
    // resume must route the error item to `recover` and leave `downstream`
    // untouched.
    let cp: Arc<dyn Checkpointer<Value>> = Arc::new(InMemoryCheckpointer::<Value>::default());
    let mut gate = node("gate", NodeKind::OutputParser);
    gate.config = json!({ "requires_approval": true });
    let graph = WorkflowGraph {
        nodes: vec![
            node("t", NodeKind::Trigger),
            gate,
            node("downstream", NodeKind::OutputParser),
            node("recover", NodeKind::OutputParser),
        ],
        edges: vec![
            edge("t", "gate"),
            port_edge("gate", "main", "downstream"),
            port_edge("gate", "error", "recover"),
        ],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");

    let caps = mock_capabilities();
    let paused = run_with_checkpointer(&compiled, json!({}), &caps, cp.clone(), "thread-deny")
        .await
        .expect("run_with_checkpointer");
    assert_eq!(paused.pending_approvals, vec!["gate".to_string()]);

    let caps = mock_capabilities();
    let journal = Arc::new(InMemoryGraphEventJournal::new());
    let observer = Arc::new(crate::observability::NoopObserver) as Arc<dyn RunObserver>;
    let denied = resume_with_checkpointer_journaled_observed(
        &compiled,
        &caps,
        cp.clone(),
        "thread-deny",
        Vec::new(),               // nothing approved
        vec!["gate".to_string()], // the gate is denied
        journal,
        &observer,
    )
    .await
    .expect("resume with rejection");

    assert!(
        denied.outcome.pending_approvals.is_empty(),
        "a denied gate is settled, not left pending"
    );
    assert_eq!(
        denied.outcome.output["nodes"]["recover"]["items"][0]["json"]["error"]["node"],
        json!("gate"),
        "the denied gate must route its error item to the `error`-port recovery node"
    );
    assert!(
        denied.outcome.output["nodes"]["downstream"].is_null(),
        "the main branch must not run when the gate is denied"
    );
}

#[tokio::test]
async fn resume_denying_a_gate_with_no_error_port_fails_the_run() {
    // trigger -> gate{requires_approval} -> downstream, with NO `error` edge.
    // Denying the gate must fail the run rather than silently swallow it.
    let cp: Arc<dyn Checkpointer<Value>> = Arc::new(InMemoryCheckpointer::<Value>::default());
    let mut gate = node("gate", NodeKind::OutputParser);
    gate.config = json!({ "requires_approval": true });
    let graph = WorkflowGraph {
        nodes: vec![
            node("t", NodeKind::Trigger),
            gate,
            node("downstream", NodeKind::OutputParser),
        ],
        edges: vec![edge("t", "gate"), edge("gate", "downstream")],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");

    let caps = mock_capabilities();
    run_with_checkpointer(&compiled, json!({}), &caps, cp.clone(), "thread-deny-fail")
        .await
        .expect("run_with_checkpointer");

    let caps = mock_capabilities();
    let journal = Arc::new(InMemoryGraphEventJournal::new());
    let observer = Arc::new(crate::observability::NoopObserver) as Arc<dyn RunObserver>;
    let result = resume_with_checkpointer_journaled_observed(
        &compiled,
        &caps,
        cp.clone(),
        "thread-deny-fail",
        Vec::new(),
        vec!["gate".to_string()],
        journal,
        &observer,
    )
    .await;
    assert!(
        result.is_err(),
        "denying a gate with no error port must fail the run"
    );
}

#[tokio::test]
async fn durable_resume_preserves_a_loop_limit_error() {
    let cp: Arc<dyn Checkpointer<Value>> = Arc::new(InMemoryCheckpointer::<Value>::default());
    let mut gate = node("gate", NodeKind::OutputParser);
    gate.config = json!({ "requires_approval": true });
    let mut loop_node = node("loop", NodeKind::Loop);
    loop_node.config = json!({ "max_iterations": 1 });
    let graph = WorkflowGraph {
        nodes: vec![
            node("t", NodeKind::Trigger),
            gate,
            loop_node,
            node("work", NodeKind::OutputParser),
        ],
        edges: vec![
            edge("t", "gate"),
            edge("gate", "loop"),
            port_edge("loop", "body", "work"),
            edge("work", "loop"),
        ],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let caps = mock_capabilities();
    let paused =
        run_with_checkpointer(&compiled, json!({}), &caps, cp.clone(), "thread-loop-limit")
            .await
            .expect("pause at approval gate");
    assert_eq!(paused.pending_approvals, vec!["gate".to_string()]);

    let caps = mock_capabilities();
    let error = resume_with_checkpointer(
        &compiled,
        &caps,
        cp,
        "thread-loop-limit",
        vec!["gate".to_string()],
    )
    .await
    .expect_err("the resumed loop should reach its cap");

    assert!(matches!(
        error,
        EngineError::LoopLimit { ref node, limit } if node == "loop" && limit == 1
    ));
}

#[tokio::test]
async fn parallel_gates_resume_one_leaves_the_other_pending() {
    // trigger fans out to two parallel gates g1 and g2 (both on the `main`
    // port), each feeding its own downstream. Resuming with only g1 approved
    // must run g1's downstream while g2 — listed in neither `approved` nor
    // `rejected` — stays pending and its downstream stays blocked. A bare
    // `true` resume value would blanket-approve g2 too and wrongly run d2.
    let graph = WorkflowGraph {
        nodes: vec![
            node("t", NodeKind::Trigger),
            gate("g1"),
            gate("g2"),
            node("d1", NodeKind::OutputParser),
            node("d2", NodeKind::OutputParser),
        ],
        edges: vec![
            edge("t", "g1"),
            edge("t", "g2"),
            edge("g1", "d1"),
            edge("g2", "d2"),
        ],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let caps = mock_capabilities();

    // Both gates pend at once. They run concurrently in the same superstep,
    // and the whole active set is folded before the run pauses, so there is
    // no reason to surface one and hide the other behind a resume round-trip
    // — a host can present both for approval immediately.
    //
    // The invariant this test guards is unchanged by that: approving g1 must
    // NOT also approve g2. A bare `true` resume value would blanket-approve
    // every interrupted gate, which is precisely what naming them prevents.
    let rr = run_resumable(&compiled, json!({}), &caps)
        .await
        .expect("run_resumable");
    let mut pending = rr.outcome().pending_approvals.clone();
    pending.sort();
    assert_eq!(
        pending,
        vec!["g1".to_string(), "g2".to_string()],
        "both parallel gates pend together"
    );

    let after_g1 = rr.resume(vec!["g1".to_string()]).await.expect("resume g1");
    assert!(
        after_g1.pending_approvals.contains(&"g2".to_string()),
        "g2 must stay pending when only g1 is approved (a bare-true resume \
             would wrongly blanket-approve it), got {:?}",
        after_g1.pending_approvals
    );
    assert!(
        after_g1.output["nodes"]["d2"].is_null(),
        "g2's downstream must NOT run while g2 is still pending"
    );

    // Resolving g2 too settles the run: no gate remains pending and g2's
    // downstream finally runs.
    let after_g2 = rr.resume(vec!["g2".to_string()]).await.expect("resume g2");
    assert!(
        after_g2.pending_approvals.is_empty(),
        "no gate pending once both parallel gates are approved, got {:?}",
        after_g2.pending_approvals
    );
    assert!(
        !after_g2.output["nodes"]["d2"]["items"].is_null(),
        "g2's downstream runs once g2 is approved"
    );
}

#[tokio::test]
async fn resume_denying_a_gate_fans_out_to_multiple_error_recovery_nodes() {
    // A denied gate whose `error` port fans out to TWO recovery nodes (≥2
    // edges on the same port) is command-routed and has no conditional router;
    // the denial must still drive BOTH recovery branches via the fan-out
    // command path rather than a plain (unrouted) update.
    let cp: Arc<dyn Checkpointer<Value>> = Arc::new(InMemoryCheckpointer::<Value>::default());
    let mut gate = node("gate", NodeKind::OutputParser);
    gate.config = json!({ "requires_approval": true });
    let graph = WorkflowGraph {
        nodes: vec![
            node("t", NodeKind::Trigger),
            gate,
            node("recover_a", NodeKind::OutputParser),
            node("recover_b", NodeKind::OutputParser),
        ],
        edges: vec![
            edge("t", "gate"),
            port_edge("gate", "error", "recover_a"),
            port_edge("gate", "error", "recover_b"),
        ],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");

    let caps = mock_capabilities();
    let paused = run_with_checkpointer(
        &compiled,
        json!({}),
        &caps,
        cp.clone(),
        "thread-fanout-deny",
    )
    .await
    .expect("run_with_checkpointer");
    assert_eq!(paused.pending_approvals, vec!["gate".to_string()]);

    let caps = mock_capabilities();
    let journal = Arc::new(InMemoryGraphEventJournal::new());
    let observer = Arc::new(crate::observability::NoopObserver) as Arc<dyn RunObserver>;
    let denied = resume_with_checkpointer_journaled_observed(
        &compiled,
        &caps,
        cp.clone(),
        "thread-fanout-deny",
        Vec::new(),
        vec!["gate".to_string()],
        journal,
        &observer,
    )
    .await
    .expect("resume with rejection");

    for recovery in ["recover_a", "recover_b"] {
        assert_eq!(
            denied.outcome.output["nodes"][recovery]["items"][0]["json"]["error"]["node"],
            json!("gate"),
            "both fan-out error-recovery branches must run on denial: {recovery}"
        );
    }
}

#[tokio::test]
async fn durable_resume_with_journal_surfaces_resume_observations() {
    // Same durable resume path as above, but with a graph event journal attached
    // to both halves. The resumed run returns its own graph run id and the
    // journal stores observations under that id.
    let cp: Arc<dyn Checkpointer<Value>> = Arc::new(InMemoryCheckpointer::<Value>::default());
    let mut approval_gate = node("gate", NodeKind::OutputParser);
    approval_gate.config = json!({ "requires_approval": true });
    let graph = WorkflowGraph {
        nodes: vec![
            node("t", NodeKind::Trigger),
            approval_gate,
            node("downstream", NodeKind::OutputParser),
        ],
        edges: vec![edge("t", "gate"), edge("gate", "downstream")],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let caps = mock_capabilities();
    let journal = Arc::new(InMemoryGraphEventJournal::new());

    let paused = run_with_checkpointer_journaled(
        &compiled,
        json!({ "request": 42 }),
        &caps,
        cp.clone(),
        "thread-journal-resume",
        journal.clone(),
    )
    .await
    .expect("journaled run");
    assert_eq!(paused.outcome.pending_approvals, vec!["gate".to_string()]);

    let resumed = resume_with_checkpointer_journaled(
        &compiled,
        &caps,
        cp.clone(),
        "thread-journal-resume",
        vec!["gate".to_string()],
        journal.clone(),
    )
    .await
    .expect("journaled resume");

    assert!(resumed.outcome.pending_approvals.is_empty());
    assert!(
        !resumed.outcome.output["nodes"]["downstream"]["items"].is_null(),
        "downstream should run during the checkpointed resume"
    );
    assert!(
        !resumed.graph_run_ids.run_id.is_empty(),
        "resume must surface the graph run id"
    );

    let observations = journal
        .read_from(&resumed.graph_run_ids.run_id, 0)
        .await
        .expect("read resume observations");
    assert!(
        !observations.is_empty(),
        "resume observations should be journaled under the resumed run id"
    );
    assert!(
        observations
            .iter()
            .any(|observation| observation.event.kind() == "run.completed"),
        "resume journal should include run completion: {observations:?}"
    );
}
