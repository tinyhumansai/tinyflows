
#[tokio::test]
async fn run_level_knobs_accepted() {
    // A trigger carrying run-level `recursion_limit` and `node_timeout_secs`
    // wired to a downstream passthrough. This proves the knobs are read from the
    // trigger config and wired onto the builder without breaking execution; the
    // downstream node still runs. (the runtime's own tests cover the knobs actually
    // firing.)
    let mut trigger = node("t", NodeKind::Trigger);
    trigger.config = json!({ "recursion_limit": 100, "node_timeout_secs": 30 });
    let graph = WorkflowGraph {
        nodes: vec![trigger, node("p", NodeKind::OutputParser)],
        edges: vec![Edge {
            from_node: "t".to_string(),
            from_port: "main".to_string(),
            to_node: "p".to_string(),
            to_port: "main".to_string(),
        }],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let caps = mock_capabilities();

    let outcome = run(&compiled, json!({ "x": 1 }), &caps).await.expect("run");
    assert_eq!(
        outcome.output["nodes"]["p"]["items"][0]["json"],
        json!({ "x": 1 })
    );
}

#[tokio::test]
async fn run_is_instrumented_and_still_succeeds() {
    // Regression guard: the `tracing` instrumentation added to `run` must not
    // alter execution. Drive a simple `trigger -> output_parser` workflow and
    // confirm the items still flow through with the instrumentation present.
    let graph = WorkflowGraph {
        nodes: vec![
            node("t", NodeKind::Trigger),
            node("p", NodeKind::OutputParser),
        ],
        edges: vec![Edge {
            from_node: "t".to_string(),
            from_port: "main".to_string(),
            to_node: "p".to_string(),
            to_port: "main".to_string(),
        }],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let caps = mock_capabilities();

    let outcome = run(&compiled, json!({ "ok": true }), &caps)
        .await
        .expect("instrumented run should still succeed");
    assert_eq!(
        outcome.output["nodes"]["p"]["items"][0]["json"],
        json!({ "ok": true })
    );
}

#[tokio::test]
async fn approval_gate_pauses_until_approved() {
    // trigger -> gate{requires_approval} -> downstream. With no approvals in
    // the input the gate must pause the run: it reports as pending and its
    // downstream never runs.
    let mut gate = node("gate", NodeKind::OutputParser);
    gate.config = json!({ "requires_approval": true });
    let graph = WorkflowGraph {
        nodes: vec![
            node("t", NodeKind::Trigger),
            gate,
            node("downstream", NodeKind::OutputParser),
        ],
        edges: vec![
            Edge {
                from_node: "t".to_string(),
                from_port: "main".to_string(),
                to_node: "gate".to_string(),
                to_port: "main".to_string(),
            },
            Edge {
                from_node: "gate".to_string(),
                from_port: "main".to_string(),
                to_node: "downstream".to_string(),
                to_port: "main".to_string(),
            },
        ],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let caps = mock_capabilities();

    let outcome = run(&compiled, json!({ "x": 1 }), &caps).await.expect("run");
    assert!(
        outcome.pending_approvals.contains(&"gate".to_string()),
        "gate should be reported as pending approval"
    );
    assert!(
        outcome.output["nodes"]["downstream"].is_null(),
        "downstream must not run while the gate is pending"
    );
}

#[tokio::test]
async fn approved_gate_completes() {
    // Same graph, but the input approves the gate: the run completes fully
    // and the downstream node runs.
    let mut gate = node("gate", NodeKind::OutputParser);
    gate.config = json!({ "requires_approval": true });
    let graph = WorkflowGraph {
        nodes: vec![
            node("t", NodeKind::Trigger),
            gate,
            node("downstream", NodeKind::OutputParser),
        ],
        edges: vec![
            Edge {
                from_node: "t".to_string(),
                from_port: "main".to_string(),
                to_node: "gate".to_string(),
                to_port: "main".to_string(),
            },
            Edge {
                from_node: "gate".to_string(),
                from_port: "main".to_string(),
                to_node: "downstream".to_string(),
                to_port: "main".to_string(),
            },
        ],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let caps = mock_capabilities();

    let outcome = run(&compiled, json!({ "approvals": ["gate"] }), &caps)
        .await
        .expect("run");
    assert!(
        outcome.pending_approvals.is_empty(),
        "no approvals should be pending once the gate is approved"
    );
    assert!(
        !outcome.output["nodes"]["downstream"]["items"].is_null(),
        "downstream should run once the gate is approved"
    );
}

/// A [`RunObserver`] that records which node ids finished and how many runs
/// started, so a test can assert the observer hooks fired.
struct Capture {
    steps: Arc<Mutex<Vec<String>>>,
    runs: Arc<Mutex<u32>>,
}

impl RunObserver for Capture {
    fn on_run_start(&self, _run_id: &str) {
        *self.runs.lock().unwrap() += 1;
    }

    fn on_step_finish(&self, step: &ExecutionStep) {
        self.steps.lock().unwrap().push(step.node_id.clone());
    }
}

#[tokio::test]
async fn observer_receives_run_start_and_step_finish() {
    // trigger -> output_parser via `run_with_observer`: on_run_start fires
    // once and on_step_finish records the (non-trigger) output_parser node.
    let graph = WorkflowGraph {
        nodes: vec![
            node("t", NodeKind::Trigger),
            node("p", NodeKind::OutputParser),
        ],
        edges: vec![Edge {
            from_node: "t".to_string(),
            from_port: "main".to_string(),
            to_node: "p".to_string(),
            to_port: "main".to_string(),
        }],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let caps = mock_capabilities();

    let steps = Arc::new(Mutex::new(Vec::new()));
    let runs = Arc::new(Mutex::new(0));
    let observer: Arc<dyn RunObserver> = Arc::new(Capture {
        steps: steps.clone(),
        runs: runs.clone(),
    });

    run_with_observer(&compiled, json!({ "x": 1 }), &caps, &observer)
        .await
        .expect("run");

    assert_eq!(*runs.lock().unwrap(), 1, "on_run_start should fire once");
    assert!(
        steps.lock().unwrap().contains(&"p".to_string()),
        "on_step_finish should record the output_parser node"
    );
}

#[tokio::test]
async fn resume_completes_a_paused_run() {
    // trigger -> gate{requires_approval} -> downstream. Running with no
    // approvals pauses at the gate; `resume` supplies the gate approval and
    // drives the run to completion so the downstream node executes.
    let gate = |id: &str| {
        let mut gate = node(id, NodeKind::OutputParser);
        gate.config = json!({ "requires_approval": true });
        gate
    };
    let graph = WorkflowGraph {
        nodes: vec![
            node("t", NodeKind::Trigger),
            gate("gate"),
            node("downstream", NodeKind::OutputParser),
        ],
        edges: vec![
            Edge {
                from_node: "t".to_string(),
                from_port: "main".to_string(),
                to_node: "gate".to_string(),
                to_port: "main".to_string(),
            },
            Edge {
                from_node: "gate".to_string(),
                from_port: "main".to_string(),
                to_node: "downstream".to_string(),
                to_port: "main".to_string(),
            },
        ],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let caps = mock_capabilities();

    let paused = run(&compiled, json!({}), &caps).await.expect("run");
    assert!(
        paused.pending_approvals.contains(&"gate".to_string()),
        "gate should be reported as pending approval"
    );
    assert!(
        paused.output["nodes"]["downstream"].is_null(),
        "downstream must not run while the gate is pending"
    );

    let done = resume(&compiled, json!({}), vec!["gate".to_string()], &caps)
        .await
        .expect("resume");
    assert!(
        done.pending_approvals.is_empty(),
        "no approvals should be pending once the gate is approved"
    );
    assert!(
        !done.output["nodes"]["downstream"]["items"].is_null(),
        "downstream should run once the gate is approved via resume"
    );
}

#[tokio::test]
async fn resume_unions_new_approval_with_existing() {
    // Two gates in series, each requiring approval. Start with `other` already
    // approved in the input and resume with `gate`: the union must preserve
    // `other` (so its gate runs) and add `gate` (so its gate runs too),
    // letting the run reach the downstream node.
    let gate = |id: &str| {
        let mut gate = node(id, NodeKind::OutputParser);
        gate.config = json!({ "requires_approval": true });
        gate
    };
    let edge = |from: &str, to: &str| Edge {
        from_node: from.to_string(),
        from_port: "main".to_string(),
        to_node: to.to_string(),
        to_port: "main".to_string(),
    };
    let graph = WorkflowGraph {
        nodes: vec![
            node("t", NodeKind::Trigger),
            gate("other"),
            gate("gate"),
            node("downstream", NodeKind::OutputParser),
        ],
        edges: vec![
            edge("t", "other"),
            edge("other", "gate"),
            edge("gate", "downstream"),
        ],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let caps = mock_capabilities();

    let done = resume(
        &compiled,
        json!({ "approvals": ["other"] }),
        vec!["gate".to_string()],
        &caps,
    )
    .await
    .expect("resume");
    assert!(
        done.pending_approvals.is_empty(),
        "unioning `gate` into the existing `other` approval should clear both gates, \
             got pending: {:?}",
        done.pending_approvals
    );
    assert!(
        !done.output["nodes"]["downstream"]["items"].is_null(),
        "downstream should run once both gates are approved via the unioned set"
    );
}

#[tokio::test]
async fn resumable_run_resumes_from_checkpoint() {
    // trigger -> gate{requires_approval} -> downstream. `run_resumable` pauses
    // at the gate and keeps the compiled graph (and its checkpointer) alive;
    // `ResumableRun::resume` then continues *from the checkpoint* — the gate is
    // approved via the delivered resume value and the downstream runs, without
    // re-executing the already-completed trigger.
    let mut gate = node("gate", NodeKind::OutputParser);
    gate.config = json!({ "requires_approval": true });
    let graph = WorkflowGraph {
        nodes: vec![
            node("t", NodeKind::Trigger),
            gate,
            node("downstream", NodeKind::OutputParser),
        ],
        edges: vec![
            Edge {
                from_node: "t".to_string(),
                from_port: "main".to_string(),
                to_node: "gate".to_string(),
                to_port: "main".to_string(),
            },
            Edge {
                from_node: "gate".to_string(),
                from_port: "main".to_string(),
                to_node: "downstream".to_string(),
                to_port: "main".to_string(),
            },
        ],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let caps = mock_capabilities();

    let rr = run_resumable(&compiled, json!({}), &caps)
        .await
        .expect("run_resumable");
    assert!(
        rr.outcome().pending_approvals.contains(&"gate".to_string()),
        "gate should be reported as pending approval"
    );
    assert!(
        rr.outcome().output["nodes"]["downstream"].is_null(),
        "downstream must not run while the gate is pending"
    );

    let done = rr.resume(vec!["gate".to_string()]).await.expect("resume");
    assert!(
        done.pending_approvals.is_empty(),
        "no approvals should be pending once the gate is resumed, got: {:?}",
        done.pending_approvals
    );
    assert!(
        !done.output["nodes"]["downstream"]["items"].is_null(),
        "downstream should run once the run resumes from the checkpoint"
    );
}

// ---- Additional comprehensive coverage ----------------------------------

/// A `main`-port edge from `from` to `to` — the common wiring in these tests.
fn edge(from: &str, to: &str) -> Edge {
    Edge {
        from_node: from.to_string(),
        from_port: "main".to_string(),
        to_node: to.to_string(),
        to_port: "main".to_string(),
    }
}

/// A `port`-port edge, for branching nodes that emit on a named port.
fn port_edge(from: &str, port: &str, to: &str) -> Edge {
    Edge {
        from_node: from.to_string(),
        from_port: port.to_string(),
        to_node: to.to_string(),
        to_port: "main".to_string(),
    }
}

/// An `output_parser` gate that requires human approval before it runs.
fn gate(id: &str) -> Node {
    let mut g = node(id, NodeKind::OutputParser);
    g.config = json!({ "requires_approval": true });
    g
}

#[tokio::test]
async fn linear_three_node_passthrough() {
    // trigger -> a -> b -> c, all output_parser passthroughs. The trigger
    // payload must flow unchanged all the way to the terminal node.
    let graph = WorkflowGraph {
        nodes: vec![
            node("t", NodeKind::Trigger),
            node("a", NodeKind::OutputParser),
            node("b", NodeKind::OutputParser),
            node("c", NodeKind::OutputParser),
        ],
        edges: vec![edge("t", "a"), edge("a", "b"), edge("b", "c")],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let caps = mock_capabilities();

    let outcome = run(&compiled, json!({ "n": 1 }), &caps).await.expect("run");
    assert_eq!(
        outcome.output["nodes"]["a"]["items"][0]["json"],
        json!({ "n": 1 })
    );
    assert_eq!(
        outcome.output["nodes"]["b"]["items"][0]["json"],
        json!({ "n": 1 })
    );
    assert_eq!(
        outcome.output["nodes"]["c"]["items"][0]["json"],
        json!({ "n": 1 })
    );
}
