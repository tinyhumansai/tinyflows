

#[tokio::test]
async fn a_loop_runs_its_body_and_exits_cleanly_at_the_cap() {
    let outcome = run_guarded(&loop_graph(
        json!({ "max_iterations": 3, "on_exceeded": "continue" }),
    ))
    .await
    .expect("the loop should finish rather than error");

    // The body ran, and the run left through `done` into the downstream node.
    assert_eq!(
        outcome.output["nodes"]["l"]["iteration"], 3,
        "the loop should have consumed exactly its three iterations"
    );
    assert_eq!(
        outcome.output["nodes"]["l"]["port"], "done",
        "the final activation exits on `done`"
    );
    assert!(
        !outcome.output["nodes"]["out"].is_null(),
        "the downstream node must run once the loop is done"
    );
}

#[tokio::test]
async fn the_default_policy_fails_the_run_at_the_cap() {
    let err = run_guarded(&loop_graph(json!({ "max_iterations": 2 })))
        .await
        .expect_err("on_exceeded defaults to `error`");
    match err {
        EngineError::LoopLimit { node, limit } => {
            assert_eq!(node, "l", "the error names the loop that ran away");
            assert_eq!(limit, 2);
        }
        other => panic!("expected LoopLimit, got: {other:?}"),
    }
}

#[tokio::test]
async fn the_default_cap_reaches_its_structured_limit() {
    let err = run_guarded(&loop_graph(json!({})))
        .await
        .expect_err("the implicit cap should stop the loop before the graph step limit");

    assert!(matches!(
        err,
        EngineError::LoopLimit { ref node, limit } if node == "l" && limit == 25
    ));
}

#[tokio::test]
async fn a_falsey_condition_exits_before_the_cap_is_reached() {
    // The condition never holds, so the loop leaves on `done` immediately and
    // the body never runs at all.
    let outcome = run_guarded(&loop_graph(json!({
        "max_iterations": 50,
        "condition": "=item.never_set"
    })))
    .await
    .expect("an early exit is not an error");

    assert_eq!(outcome.output["nodes"]["l"]["iteration"], 0);
    assert!(
        outcome.output["nodes"]["work"].is_null(),
        "the body must not run when the condition is false on the first pass"
    );
    assert!(
        !outcome.output["nodes"]["out"].is_null(),
        "the run still completes through `done`"
    );
}

/// The regression that motivated the feature: a cycle closing on a *mid-graph*
/// node. Such a node has two predecessors, and the engine used to lower its
/// incoming edges as a fan-in merge barrier — which waited forever for the
/// back-edge that could only arrive after the node itself ran. The run settled
/// as `Ok` with only the trigger having executed.
#[tokio::test]
async fn a_plain_back_edge_onto_a_mid_graph_node_iterates() {
    let graph = WorkflowGraph {
        name: "plain_cycle".to_string(),
        nodes: vec![
            node("t", NodeKind::Trigger, json!({ "recursion_limit": 6 })),
            node("a", NodeKind::OutputParser, Value::Null),
            node("b", NodeKind::OutputParser, Value::Null),
        ],
        edges: vec![edge("t", "a"), edge("a", "b"), edge("b", "a")],
        ..Default::default()
    };

    // With no loop node to bound it, the graph-wide recursion limit is what
    // stops the run — and reaching it proves the cycle actually iterated.
    let err = run_guarded(&graph)
        .await
        .expect_err("an unbounded cycle must hit the recursion limit");
    assert!(
        !matches!(err, EngineError::Validation(_)),
        "the graph is legal; it should fail at run time, not validation: {err:?}"
    );
}

/// A loop head that is **also** a fan-in iterates properly.
///
/// This used to be refused. The barrier a fan-in installs is keyed on the
/// target node rather than on the edge, so the re-entry the back-edge delivered
/// was tested against the head's *forward* predecessors, failed that test, and
/// was dropped — the loop ran exactly one pass and stopped. The gate now
/// ignores arrivals from predecessors outside its required set, and a
/// back-edge's source is never in that set, so the re-entry lands.
///
/// The assertion that matters is the iteration count: a passing run that
/// stopped after one pass would still reach `out`, so only the count
/// distinguishes a fixed loop from the old silent failure.
#[tokio::test]
async fn a_loop_head_that_is_also_a_fan_in_iterates() {
    let graph = WorkflowGraph {
        name: "fan_in_loop".to_string(),
        nodes: vec![
            node("t", NodeKind::Trigger, Value::Null),
            node("left", NodeKind::OutputParser, Value::Null),
            node("right", NodeKind::OutputParser, Value::Null),
            node(
                "l",
                NodeKind::Loop,
                json!({ "max_iterations": 2, "on_exceeded": "continue" }),
            ),
            node("work", NodeKind::OutputParser, Value::Null),
            node("out", NodeKind::OutputParser, Value::Null),
        ],
        edges: vec![
            edge("t", "left"),
            edge("t", "right"),
            edge("left", "l"),
            edge("right", "l"),
            port_edge("l", "body", "work"),
            edge("work", "l"),
            port_edge("l", "done", "out"),
        ],
        ..Default::default()
    };

    let errors = tinyflows::validate::validate_all(&graph);
    assert!(
        errors.is_empty(),
        "a fan-in loop head is legal now that the barrier ignores back-edge \
         arrivals, got: {errors:?}"
    );

    let outcome = run_guarded(&graph)
        .await
        .expect("the loop should run rather than deadlock");

    assert_eq!(
        outcome.output["nodes"]["l"]["iteration"], 2,
        "the loop should consume both its iterations; a count of 1 means the \
         back-edge re-entry was swallowed by the fan-in barrier again"
    );
    assert!(
        outcome.output["nodes"]["out"].get("items").is_some(),
        "the loop should still leave through `done` into the downstream node"
    );
}

/// The fix for the case above: join the arms *before* the loop head, so the
/// head keeps a single forward predecessor and the merge sits outside the
/// cycle. This is the shape the validation error points authors at.
#[tokio::test]
async fn a_fan_in_joined_before_the_loop_head_iterates() {
    let graph = WorkflowGraph {
        name: "fan_in_then_loop".to_string(),
        nodes: vec![
            node("t", NodeKind::Trigger, Value::Null),
            node("left", NodeKind::OutputParser, Value::Null),
            node("right", NodeKind::OutputParser, Value::Null),
            node("j", NodeKind::Merge, Value::Null),
            node(
                "l",
                NodeKind::Loop,
                json!({ "max_iterations": 2, "on_exceeded": "continue" }),
            ),
            node("work", NodeKind::OutputParser, Value::Null),
            node("out", NodeKind::OutputParser, Value::Null),
        ],
        edges: vec![
            edge("t", "left"),
            edge("t", "right"),
            edge("left", "j"),
            edge("right", "j"),
            edge("j", "l"),
            port_edge("l", "body", "work"),
            edge("work", "l"),
            port_edge("l", "done", "out"),
        ],
        ..Default::default()
    };

    assert!(
        tinyflows::validate::validate_all(&graph).is_empty(),
        "joining before the head is the supported shape"
    );
    let outcome = run_guarded(&graph).await.expect("the loop should finish");
    assert!(
        !outcome.output["nodes"]["left"].is_null() && !outcome.output["nodes"]["right"].is_null(),
        "both arms of the fan-in must run"
    );
    assert_eq!(outcome.output["nodes"]["l"]["iteration"], 2);
    assert!(!outcome.output["nodes"]["out"].is_null());
}

/// A single-input `merge` inside the loop body is a passthrough, not a fan-in
/// barrier, so it can iterate normally.
#[tokio::test]
async fn a_single_input_merge_inside_the_loop_body_iterates() {
    let graph = WorkflowGraph {
        name: "merge_in_body".to_string(),
        nodes: vec![
            node("t", NodeKind::Trigger, Value::Null),
            node(
                "l",
                NodeKind::Loop,
                json!({ "max_iterations": 2, "on_exceeded": "continue" }),
            ),
            node("m", NodeKind::Merge, Value::Null),
            node("out", NodeKind::OutputParser, Value::Null),
        ],
        edges: vec![
            edge("t", "l"),
            port_edge("l", "body", "m"),
            edge("m", "l"),
            port_edge("l", "done", "out"),
        ],
        ..Default::default()
    };

    assert!(tinyflows::validate::validate_all(&graph).is_empty());
    let outcome = run_guarded(&graph).await.expect("the loop should finish");
    assert_eq!(outcome.output["nodes"]["l"]["iteration"], 2);
    assert!(!outcome.output["nodes"]["out"].is_null());
}

/// An unbounded cycle — no `loop` node counting passes and no
/// `recursion_limit` on the trigger — is refused, so the bound is an authoring
/// decision rather than whatever the host's wall clock happens to be.
#[tokio::test]
async fn an_unbounded_cycle_is_refused() {
    let graph = WorkflowGraph {
        name: "unbounded".to_string(),
        nodes: vec![
            node("t", NodeKind::Trigger, Value::Null),
            node("a", NodeKind::OutputParser, Value::Null),
            node("b", NodeKind::OutputParser, Value::Null),
        ],
        edges: vec![edge("t", "a"), edge("a", "b"), edge("b", "a")],
        ..Default::default()
    };

    let errors = tinyflows::validate::validate_all(&graph);
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, ValidationError::IllegalCycle(_))),
        "an uncapped cycle should be refused, got: {errors:?}"
    );
}

/// A loop can read its own pass number from anywhere in the graph, which is
/// what makes an iteration-aware body possible.
#[tokio::test]
async fn the_iteration_count_is_readable_from_an_expression() {
    let graph = WorkflowGraph {
        name: "iteration_expr".to_string(),
        nodes: vec![
            node("t", NodeKind::Trigger, Value::Null),
            node(
                "l",
                NodeKind::Loop,
                json!({ "max_iterations": 3, "on_exceeded": "continue" }),
            ),
            node(
                "work",
                NodeKind::Transform,
                json!({ "set": { "pass": "=nodes.l.iteration" } }),
            ),
            node("out", NodeKind::OutputParser, Value::Null),
        ],
        edges: vec![
            edge("t", "l"),
            port_edge("l", "body", "work"),
            edge("work", "l"),
            port_edge("l", "done", "out"),
        ],
        ..Default::default()
    };

    let outcome = run_guarded(&graph).await.expect("the loop should finish");
    // The body's last pass saw the third iteration, so the counter reached the
    // body rather than resolving null.
    assert_eq!(
        outcome.output["nodes"]["work"]["items"][0]["json"]["pass"], 3,
        "the body should see the current pass number"
    );
}

/// `max_node_visits` is the backstop under a `loop` node: it bounds a cycle
/// nobody declared a loop node for, and unlike `recursion_limit` the failure
/// names the node that ran away rather than just reporting too many steps.
#[tokio::test]
async fn max_node_visits_bounds_a_cycle_and_names_the_node() {
    let graph = WorkflowGraph {
        name: "visit_capped".to_string(),
        nodes: vec![
            node(
                "t",
                NodeKind::Trigger,
                json!({ "recursion_limit": 500, "max_node_visits": 3 }),
            ),
            node("a", NodeKind::OutputParser, Value::Null),
            node("b", NodeKind::OutputParser, Value::Null),
        ],
        edges: vec![edge("t", "a"), edge("a", "b"), edge("b", "a")],
        ..Default::default()
    };

    let err = run_guarded(&graph)
        .await
        .expect_err("the visit cap must stop the cycle");
    let message = err.to_string();
    assert!(
        message.contains('a') && message.to_lowercase().contains("visit"),
        "the failure should name the node and its visit cap, got: {message}"
    );
}

/// A diamond **inside** the loop body iterates, and the merge fires once per
/// pass rather than on stale data.
///
/// Two things had to be true for this. The barrier re-arms on its own, because
/// arrivals are cleared when it fires. And barrier *relief* had to stop firing
/// phantom arrivals here: the arms are reachable only through the head's `body`
/// port, so relief is registered for them, and relief's forward walk used to
/// stop at the fan-out (`apex`) because a command node has no static edge. It
/// concluded the arms were untaken and cleared the barrier early — the observed
/// order was `l, apex, join, arm_a, arm_b, …`, the join reading the previous
/// pass's data. Silently wrong output, not a hang.
///
/// So this test asserts both the iteration count *and* the ordering: `join`
/// must never run before both arms have, on any pass.
#[tokio::test]
async fn a_diamond_inside_the_loop_body_iterates() {
    let graph = WorkflowGraph {
        name: "diamond_in_loop".to_string(),
        nodes: vec![
            node("t", NodeKind::Trigger, Value::Null),
            node(
                "l",
                NodeKind::Loop,
                json!({ "max_iterations": 3, "on_exceeded": "continue" }),
            ),
            node("apex", NodeKind::OutputParser, Value::Null),
            node(
                "arm_a",
                NodeKind::Transform,
                json!({ "set": { "arm": "a" } }),
            ),
            node(
                "arm_b",
                NodeKind::Transform,
                json!({ "set": { "arm": "b" } }),
            ),
            node("join", NodeKind::Merge, Value::Null),
            node("out", NodeKind::OutputParser, Value::Null),
        ],
        edges: vec![
            edge("t", "l"),
            port_edge("l", "body", "apex"),
            // Both arms leave `apex` on the same port: a parallel fan-out.
            edge("apex", "arm_a"),
            edge("apex", "arm_b"),
            edge("arm_a", "join"),
            edge("arm_b", "join"),
            edge("join", "l"), // the back-edge closing the cycle
            port_edge("l", "done", "out"),
        ],
        ..Default::default()
    };

    let errors = tinyflows::validate::validate_all(&graph);
    assert!(
        errors.is_empty(),
        "a diamond whose arms are both on the cycle is legal, got: {errors:?}"
    );

    let trace = Arc::new(Trace::default());
    let observer: Arc<dyn RunObserver> = trace.clone();
    let caps = mock_capabilities();
    let compiled = compile(&graph).expect("compile");
    let outcome = tokio::time::timeout(
        GUARD,
        tinyflows::engine::run_with_observer(&compiled, json!({}), &caps, &observer),
    )
    .await
    .expect("run hung — the diamond deadlocked the loop")
    .expect("the loop should iterate rather than fail");

    assert_eq!(
        outcome.output["nodes"]["l"]["iteration"], 3,
        "every pass should complete the merge barrier"
    );

    // Ordering: walking the activation trace, `join` may only run when both
    // arms have run since the last time it did. A `join` that fires early is
    // the phantom-arrival bug, and it does not show up in the final state.
    let order = trace.0.lock().expect("trace mutex poisoned").clone();
    let (mut seen_a, mut seen_b, mut joins) = (false, false, 0);
    for id in &order {
        match id.as_str() {
            "arm_a" => seen_a = true,
            "arm_b" => seen_b = true,
            "join" => {
                assert!(
                    seen_a && seen_b,
                    "`join` fired before both arms had run on this pass — barrier \
                     relief cleared the barrier early. Trace: {order:?}"
                );
                joins += 1;
                seen_a = false;
                seen_b = false;
            }
            _ => {}
        }
    }
    assert_eq!(
        joins, 3,
        "the merge should fire once per pass. Trace: {order:?}"
    );
}
