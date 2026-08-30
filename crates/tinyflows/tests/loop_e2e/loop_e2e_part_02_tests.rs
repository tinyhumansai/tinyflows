
/// The case that stays refused: a merge on the cycle that also waits on a
/// predecessor from **outside** the cycle.
///
/// The off-cycle arm runs once, on the seeding pass, and never activates again,
/// so from the second iteration the barrier can never complete its required set
/// and the loop stops dead. Refusing it beats hanging.
#[tokio::test]
async fn a_merge_on_the_cycle_waiting_on_an_off_cycle_arm_is_refused() {
    let graph = WorkflowGraph {
        name: "off_cycle_arm".to_string(),
        nodes: vec![
            node("t", NodeKind::Trigger, Value::Null),
            // `outside` runs once from the trigger and is not on the cycle.
            node("outside", NodeKind::OutputParser, Value::Null),
            node(
                "l",
                NodeKind::Loop,
                json!({ "max_iterations": 3, "on_exceeded": "continue" }),
            ),
            node("work", NodeKind::OutputParser, Value::Null),
            node("join", NodeKind::Merge, Value::Null),
            node("out", NodeKind::OutputParser, Value::Null),
        ],
        edges: vec![
            edge("t", "l"),
            edge("t", "outside"),
            port_edge("l", "body", "work"),
            edge("work", "join"),
            edge("outside", "join"), // the off-cycle arm
            edge("join", "l"),
            port_edge("l", "done", "out"),
        ],
        ..Default::default()
    };

    let errors = tinyflows::validate::validate_all(&graph);
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, ValidationError::IllegalCycle(id) if id == "join")),
        "a merge waiting on an off-cycle arm should still be refused, got: {errors:?}"
    );
}

/// The accumulator survives across iterations: each pass folds the body's
/// output into state that the next pass reads back.
///
/// This is the capability the node gained — without it a loop passes its input
/// straight through and cannot remember what it tried.
#[tokio::test]
async fn an_accumulator_survives_across_iterations() {
    let graph = loop_graph(json!({
        "max_iterations": 3,
        "on_exceeded": "continue",
        "state": {
            "init": { "attempts": [] },
            // Append the pass number each time round.
            "update": "={ attempts: (.state.attempts + [((.state.attempts | length) + 1)]) }"
        }
    }));

    let outcome = run_guarded(&graph).await.expect("run");
    assert_eq!(
        outcome.output["nodes"]["l"]["state"]["attempts"],
        json!([1, 2, 3]),
        "one append per pass, carried across iterations"
    );
}

/// A key can be *removed* from the accumulator.
///
/// The regression test for the `$replace` sentinel. Under a plain deep merge an
/// object slot can only ever gain keys, so an accumulator could never drop one
/// — an error recorded on pass 1 would haunt every later pass.
#[tokio::test]
async fn an_accumulator_can_drop_a_key_it_previously_held() {
    let graph = loop_graph(json!({
        "max_iterations": 2,
        "on_exceeded": "continue",
        "state": {
            "init": { "err": "boom", "tries": 0 },
            // Rebuild the accumulator without `err`.
            "update": "={ tries: (.state.tries + 1) }"
        }
    }));

    let outcome = run_guarded(&graph).await.expect("run");
    let state = &outcome.output["nodes"]["l"]["state"];
    assert_eq!(state["tries"], 2, "the surviving key still accumulates");
    assert!(
        state.get("err").is_none(),
        "the dropped key must actually be gone, got: {state}"
    );
}

/// `until` exits as soon as the check passes, before the cap, and says so.
#[tokio::test]
async fn until_exits_early_and_reports_its_reason() {
    let graph = loop_graph(json!({
        "max_iterations": 10,
        "on_exceeded": "continue",
        "state": { "init": { "tries": 0 }, "update": "={ tries: (.state.tries + 1) }" },
        "until": "=.state.tries >= 2"
    }));

    let outcome = run_guarded(&graph).await.expect("run");
    assert_eq!(outcome.output["nodes"]["l"]["exit_reason"], "until");
    assert_eq!(
        outcome.output["nodes"]["l"]["iteration"], 2,
        "it should stop at the check, well short of the cap of 10"
    );
}

/// An `until` that never passes falls through to the cap, and the exit reason
/// distinguishes that from converging.
///
/// This is what finally makes `on_exceeded: "continue"` usable: downstream
/// could not previously tell a loop that succeeded from one that ran out.
#[tokio::test]
async fn an_until_that_never_passes_reports_exhaustion_not_success() {
    let graph = loop_graph(json!({
        "max_iterations": 2,
        "on_exceeded": "continue",
        "state": { "init": { "tries": 0 }, "update": "={ tries: (.state.tries + 1) }" },
        "until": "=.state.tries >= 99"
    }));

    let outcome = run_guarded(&graph).await.expect("run");
    assert_eq!(
        outcome.output["nodes"]["l"]["exit_reason"],
        "max_iterations"
    );
}

/// `success_port: true` routes a converged exit away from an exhausted one, so
/// the two outcomes can be handled differently.
#[tokio::test]
async fn the_success_port_separates_convergence_from_exhaustion() {
    let mut graph = loop_graph(json!({
        "max_iterations": 10,
        "on_exceeded": "continue",
        "success_port": true,
        "state": { "init": { "tries": 0 }, "update": "={ tries: (.state.tries + 1) }" },
        "until": "=.state.tries >= 2"
    }));
    graph
        .nodes
        .push(node("won", NodeKind::OutputParser, Value::Null));
    graph.edges.push(port_edge("l", "success", "won"));

    let outcome = run_guarded(&graph).await.expect("run");
    assert!(
        !outcome.output["nodes"]["won"]["items"].is_null(),
        "a converged loop leaves through `success`"
    );
    assert!(
        outcome.output["nodes"]["out"].is_null(),
        "and not through `done`, which is the exhaustion path"
    );
}

/// The accumulator is addressable from anywhere in the graph, like the
/// iteration count — including from inside the loop body.
#[tokio::test]
async fn the_body_can_read_the_accumulator() {
    let mut graph = loop_graph(json!({
        "max_iterations": 2,
        "on_exceeded": "continue",
        "state": { "init": { "tries": 0 }, "update": "={ tries: (.state.tries + 1) }" }
    }));
    // Replace the body with a transform that stamps what it can see.
    graph.nodes.retain(|n| n.id != "work");
    graph.nodes.push(node(
        "work",
        NodeKind::Transform,
        json!({ "set": { "seen": "=nodes.l.state.tries" } }),
    ));

    let outcome = run_guarded(&graph).await.expect("run");
    assert!(
        !outcome.output["nodes"]["work"]["items"].is_null(),
        "the body ran and could resolve =nodes.l.state.tries"
    );
}

/// `emit: "state"` puts the accumulator on the exit port, so downstream
/// receives what the loop built rather than the last pass's items.
#[tokio::test]
async fn emit_state_puts_the_accumulator_on_the_done_port() {
    let graph = loop_graph(json!({
        "max_iterations": 2,
        "on_exceeded": "continue",
        "emit": "state",
        "state": { "init": { "tries": 0 }, "update": "={ tries: (.state.tries + 1) }" }
    }));

    let outcome = run_guarded(&graph).await.expect("run");
    let items = outcome.output["nodes"]["out"]["items"]
        .as_array()
        .expect("downstream items");
    assert_eq!(items.len(), 1);
    assert_eq!(
        items[0]["json"]["tries"], 2,
        "downstream got the accumulator"
    );
}

/// A loop with no `state` config behaves exactly as before.
///
/// The accumulator is additive: every existing graph must be unaffected.
#[tokio::test]
async fn a_loop_without_an_accumulator_is_unchanged() {
    let outcome = run_guarded(&loop_graph(
        json!({ "max_iterations": 3, "on_exceeded": "continue" }),
    ))
    .await
    .expect("run");

    assert_eq!(outcome.output["nodes"]["l"]["iteration"], 3);
    assert_eq!(outcome.output["nodes"]["l"]["port"], "done");
    assert!(
        !outcome.output["nodes"]["out"].is_null(),
        "downstream still receives the last pass's items"
    );
}

/// A cycle bounded only by `max_node_visits` validates.
///
/// It used to be refused: only `recursion_limit` counted as proof of
/// boundedness, so a graph that genuinely could not run away was rejected — and
/// the author was pushed toward the *less* informative knob, since
/// `recursion_limit` can only report that the run looped while
/// `max_node_visits` names the node that did.
#[tokio::test]
async fn a_cycle_bounded_only_by_max_node_visits_is_legal() {
    let graph = WorkflowGraph {
        name: "visits_bounded".to_string(),
        nodes: vec![
            node("t", NodeKind::Trigger, json!({ "max_node_visits": 4 })),
            node("a", NodeKind::OutputParser, Value::Null),
            node("b", NodeKind::OutputParser, Value::Null),
        ],
        edges: vec![edge("t", "a"), edge("a", "b"), edge("b", "a")],
        ..Default::default()
    };

    let errors = tinyflows::validate::validate_all(&graph);
    assert!(
        errors.is_empty(),
        "max_node_visits bounds the cycle, so the graph is legal, got: {errors:?}"
    );

    // And the bound really is enforced, naming the runaway node.
    let err = run_guarded(&graph)
        .await
        .expect_err("the visit cap should stop the cycle");
    let message = err.to_string();
    assert!(
        message.contains('a') && message.to_lowercase().contains("visit"),
        "the failure should name the node and its visit cap, got: {message}"
    );
}

/// A cycle with neither run-level bound nor a `loop` node is still refused —
/// the lift widened what counts as a bound, it did not remove the requirement.
#[tokio::test]
async fn an_unbounded_cycle_is_still_refused_after_the_lift() {
    let graph = WorkflowGraph {
        name: "still_unbounded".to_string(),
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
        "a cycle with no bound at all must still be refused, got: {errors:?}"
    );
}

/// `success_port: true` with nothing wired to `success` is refused, because a
/// converged loop would otherwise strand the run at an unwired port — which
/// reads as "the loop never finished" rather than as a wiring mistake.
#[tokio::test]
async fn an_unwired_success_port_is_refused() {
    let graph = loop_graph(json!({
        "max_iterations": 3,
        "success_port": true,
        "until": "=true"
    }));
    let errors = tinyflows::validate::validate_all(&graph);
    assert!(
        errors.iter().any(|e| matches!(
            e,
            ValidationError::InvalidNodeConfig { node, reason }
                if node == "l" && reason.contains("success")
        )),
        "an unwired success port should be refused, got: {errors:?}"
    );
}

/// The new config keys are checked at author time rather than silently ignored.
#[tokio::test]
async fn malformed_accumulator_config_is_refused() {
    for bad in [
        json!({ "max_iterations": 2, "state": "not an object" }),
        json!({ "max_iterations": 2, "emit": "sideways" }),
    ] {
        let errors = tinyflows::validate::validate_all(&loop_graph(bad.clone()));
        assert!(
            errors.iter().any(
                |e| matches!(e, ValidationError::InvalidNodeConfig { node, .. } if node == "l")
            ),
            "config {bad} should be refused, got: {errors:?}"
        );
    }
}
