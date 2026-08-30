
#[tokio::test]
async fn condition_truthy_takes_true_branch_only() {
    // condition(field=active) with a truthy input runs only the `true` branch.
    let mut condition = node("c", NodeKind::Condition);
    condition.config = json!({ "field": "active" });
    let graph = WorkflowGraph {
        nodes: vec![
            node("t", NodeKind::Trigger),
            condition,
            node("yes", NodeKind::OutputParser),
            node("no", NodeKind::OutputParser),
        ],
        edges: vec![
            edge("t", "c"),
            port_edge("c", "true", "yes"),
            port_edge("c", "false", "no"),
        ],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let caps = mock_capabilities();

    let outcome = run(&compiled, json!({ "active": true }), &caps)
        .await
        .expect("run");
    assert!(
        !outcome.output["nodes"]["yes"]["items"].is_null(),
        "true branch must run for a truthy input"
    );
    assert!(
        outcome.output["nodes"]["no"].is_null(),
        "false branch must not run for a truthy input"
    );
}

#[tokio::test]
async fn condition_falsey_takes_false_branch_only() {
    // condition(field=active) with a falsey input runs only the `false` branch.
    let mut condition = node("c", NodeKind::Condition);
    condition.config = json!({ "field": "active" });
    let graph = WorkflowGraph {
        nodes: vec![
            node("t", NodeKind::Trigger),
            condition,
            node("yes", NodeKind::OutputParser),
            node("no", NodeKind::OutputParser),
        ],
        edges: vec![
            edge("t", "c"),
            port_edge("c", "true", "yes"),
            port_edge("c", "false", "no"),
        ],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let caps = mock_capabilities();

    let outcome = run(&compiled, json!({ "active": false }), &caps)
        .await
        .expect("run");
    assert!(
        outcome.output["nodes"]["yes"].is_null(),
        "true branch must not run for a falsey input"
    );
    assert!(
        !outcome.output["nodes"]["no"]["items"].is_null(),
        "false branch must run for a falsey input"
    );
}

#[tokio::test]
async fn condition_without_field_uses_whole_item() {
    // No `field`: the whole (non-empty) input item is the truthiness subject,
    // so a non-empty object routes to the `true` branch.
    let graph = WorkflowGraph {
        nodes: vec![
            node("t", NodeKind::Trigger),
            node("c", NodeKind::Condition),
            node("yes", NodeKind::OutputParser),
            node("no", NodeKind::OutputParser),
        ],
        edges: vec![
            edge("t", "c"),
            port_edge("c", "true", "yes"),
            port_edge("c", "false", "no"),
        ],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let caps = mock_capabilities();

    let outcome = run(&compiled, json!({ "x": 1 }), &caps).await.expect("run");
    assert!(
        !outcome.output["nodes"]["yes"]["items"].is_null(),
        "a non-empty object item is truthy and routes true"
    );
    assert!(
        outcome.output["nodes"]["no"].is_null(),
        "false branch must not run"
    );
}

#[tokio::test]
async fn condition_single_true_edge_filters_on_false() {
    // Regression for B15: a `condition` wired with only a `true` edge (no
    // `false` edge — the common "gate/filter" shape) used to be lowered as
    // an UNCONDITIONAL plain edge, so `sink` ran on *every* execution —
    // including when the condition emitted `false` — but with an EMPTY
    // input, since `collect_input` port-matches the edge's `true` against
    // the emitted `false` and drops the items. It must now act as a FILTER:
    // run `sink` (with items) when the branch is taken, and terminate the
    // run to END — never running `sink` at all — when it isn't.
    let mut condition = node("c", NodeKind::Condition);
    condition.config = json!({ "field": "active" });
    let graph = WorkflowGraph {
        nodes: vec![
            node("t", NodeKind::Trigger),
            condition,
            node("sink", NodeKind::OutputParser),
        ],
        edges: vec![edge("t", "c"), port_edge("c", "true", "sink")],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let caps = mock_capabilities();

    let truthy = run(&compiled, json!({ "active": true }), &caps)
        .await
        .expect("run");
    assert_eq!(
        truthy.output["nodes"]["sink"]["items"][0]["json"],
        json!({ "active": true }),
        "true branch must run sink WITH the item, not an empty input"
    );

    let falsey = run(&compiled, json!({ "active": false }), &caps)
        .await
        .expect("run");
    assert!(
        falsey.output["nodes"]["sink"].is_null(),
        "false branch must terminate the run to END without running sink"
    );
}

#[tokio::test]
async fn condition_single_true_edge_item_flows_through() {
    // trigger -> split_out (per-item fan-out) -> condition(field="assignee",
    // true-only edge) -> sink. Proves the B15 fix end-to-end through a
    // realistic shape: a downstream node's `=item.<field>` must see the real
    // item, not null, when the guarding condition took the `true` branch.
    let mut condition = node("c", NodeKind::Condition);
    condition.config = json!({ "field": "assignee" });
    let graph = WorkflowGraph {
        nodes: vec![
            node("t", NodeKind::Trigger),
            node("s", NodeKind::SplitOut),
            condition,
            node("sink", NodeKind::OutputParser),
        ],
        edges: vec![
            edge("t", "s"),
            edge("s", "c"),
            port_edge("c", "true", "sink"),
        ],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let caps = mock_capabilities();

    // Truthy assignee: the item must flow all the way through to `sink`.
    let assigned = run(&compiled, json!({ "assignee": "alice" }), &caps)
        .await
        .expect("run");
    assert_eq!(
        assigned.output["nodes"]["sink"]["items"][0]["json"]["assignee"],
        json!("alice"),
        "true branch must carry the real item through — not starve it to null"
    );

    // Missing assignee (falsey): `sink` must not run at all.
    let unassigned = run(&compiled, json!({}), &caps).await.expect("run");
    assert!(
        unassigned.output["nodes"]["sink"].is_null(),
        "false branch must not execute the guarded successor"
    );
}

#[tokio::test]
async fn switch_field_matching_case_routes_there() {
    // switch(field=kind) with input kind="a" routes only to the `a` case; the
    // `default` fallback does not run.
    let mut switch = node("sw", NodeKind::Switch);
    switch.config = json!({ "field": "kind" });
    let graph = WorkflowGraph {
        nodes: vec![
            node("t", NodeKind::Trigger),
            switch,
            node("case_a", NodeKind::OutputParser),
            node("fallback", NodeKind::OutputParser),
        ],
        edges: vec![
            edge("t", "sw"),
            port_edge("sw", "a", "case_a"),
            port_edge("sw", "default", "fallback"),
        ],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let caps = mock_capabilities();

    let outcome = run(&compiled, json!({ "kind": "a" }), &caps)
        .await
        .expect("run");
    assert!(
        !outcome.output["nodes"]["case_a"]["items"].is_null(),
        "matching `a` case must run"
    );
    assert!(
        outcome.output["nodes"]["fallback"].is_null(),
        "default fallback must not run when a case matches"
    );
}

#[tokio::test]
async fn switch_no_match_routes_to_default() {
    // switch(field=kind) with a missing `kind` yields a null case value, which
    // the impl maps to the `default` port; only the fallback runs.
    let mut switch = node("sw", NodeKind::Switch);
    switch.config = json!({ "field": "kind" });
    let graph = WorkflowGraph {
        nodes: vec![
            node("t", NodeKind::Trigger),
            switch,
            node("case_a", NodeKind::OutputParser),
            node("fallback", NodeKind::OutputParser),
        ],
        edges: vec![
            edge("t", "sw"),
            port_edge("sw", "a", "case_a"),
            port_edge("sw", "default", "fallback"),
        ],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let caps = mock_capabilities();

    let outcome = run(&compiled, json!({ "other": "z" }), &caps)
        .await
        .expect("run");
    assert!(
        outcome.output["nodes"]["case_a"].is_null(),
        "no case matches, so the `a` branch must not run"
    );
    assert!(
        !outcome.output["nodes"]["fallback"]["items"].is_null(),
        "a null case value routes to the default fallback"
    );
}

#[tokio::test]
async fn parallel_fan_out_of_three_runs_all() {
    // trigger fans out on port `main` to three successors; all must run.
    let graph = WorkflowGraph {
        nodes: vec![
            node("t", NodeKind::Trigger),
            node("a", NodeKind::OutputParser),
            node("b", NodeKind::OutputParser),
            node("c", NodeKind::OutputParser),
        ],
        edges: vec![edge("t", "a"), edge("t", "b"), edge("t", "c")],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let caps = mock_capabilities();

    let outcome = run(&compiled, json!({ "v": 1 }), &caps).await.expect("run");
    for id in ["a", "b", "c"] {
        assert!(
            !outcome.output["nodes"][id]["items"].is_null(),
            "fan-out branch {id} should have run"
        );
    }
}

#[tokio::test]
async fn merge_fan_in_concatenates_three_items() {
    // trigger -> d, which fans out to a, b, c (each a passthrough of the single
    // trigger item); all three feed merge `m`. The barrier holds until all
    // three complete, and merge concatenates their items => exactly 3 items.
    let graph = WorkflowGraph {
        nodes: vec![
            node("t", NodeKind::Trigger),
            node("d", NodeKind::OutputParser),
            node("a", NodeKind::OutputParser),
            node("b", NodeKind::OutputParser),
            node("c", NodeKind::OutputParser),
            node("m", NodeKind::Merge),
        ],
        edges: vec![
            edge("t", "d"),
            edge("d", "a"),
            edge("d", "b"),
            edge("d", "c"),
            edge("a", "m"),
            edge("b", "m"),
            edge("c", "m"),
        ],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let caps = mock_capabilities();

    let outcome = run(&compiled, json!({ "v": 1 }), &caps).await.expect("run");
    let merged = outcome.output["nodes"]["m"]["items"]
        .as_array()
        .expect("merge should have produced an items array");
    assert_eq!(
        merged.len(),
        3,
        "merge should concatenate one item from each of the 3 branches"
    );
}

#[tokio::test]
async fn diamond_merge_produces_two_items() {
    // Diamond: trigger -> d, d fans out to a & b, both merge at m, then m -> done.
    // The merge sees exactly 2 items and passes them to the node past the barrier.
    let graph = WorkflowGraph {
        nodes: vec![
            node("t", NodeKind::Trigger),
            node("d", NodeKind::OutputParser),
            node("a", NodeKind::OutputParser),
            node("b", NodeKind::OutputParser),
            node("m", NodeKind::Merge),
            node("done", NodeKind::OutputParser),
        ],
        edges: vec![
            edge("t", "d"),
            edge("d", "a"),
            edge("d", "b"),
            edge("a", "m"),
            edge("b", "m"),
            edge("m", "done"),
        ],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let caps = mock_capabilities();

    let outcome = run(&compiled, json!({ "v": 1 }), &caps).await.expect("run");
    assert_eq!(
        outcome.output["nodes"]["m"]["items"]
            .as_array()
            .expect("merge items")
            .len(),
        2,
        "two branches merge into two items"
    );
    assert_eq!(
        outcome.output["nodes"]["done"]["items"]
            .as_array()
            .expect("done items")
            .len(),
        2,
        "the node past the barrier receives both merged items"
    );
}

#[tokio::test]
async fn on_error_stop_fails_the_run() {
    // A tool_call with no `slug` errors deterministically; the default `stop`
    // policy makes the whole run return Err.
    let graph = WorkflowGraph {
        nodes: vec![node("t", NodeKind::Trigger), node("x", NodeKind::ToolCall)],
        edges: vec![edge("t", "x")],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let caps = mock_capabilities();

    assert!(
        run(&compiled, json!({}), &caps).await.is_err(),
        "a failing node under the default stop policy must fail the run"
    );
}

#[tokio::test]
async fn on_error_continue_completes_with_error_item() {
    // `on_error: continue` turns the failure into an error item on the default
    // port and lets the run complete Ok.
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
    assert_eq!(
        outcome.output["nodes"]["x"]["items"][0]["json"]["error"]["node"],
        json!("x")
    );
}

#[tokio::test]
async fn on_error_route_delivers_error_item_to_recovery_node() {
    // `on_error: route` emits the error item on the `error` port; a recovery
    // node wired from that port receives it, and the main-port branch does not.
    let mut tool = node("x", NodeKind::ToolCall);
    tool.config = json!({ "on_error": "route" });
    let graph = WorkflowGraph {
        nodes: vec![
            node("t", NodeKind::Trigger),
            tool,
            node("recover", NodeKind::OutputParser),
            node("normal", NodeKind::OutputParser),
        ],
        edges: vec![
            edge("t", "x"),
            port_edge("x", "error", "recover"),
            port_edge("x", "main", "normal"),
        ],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let caps = mock_capabilities();

    let outcome = run(&compiled, json!({}), &caps).await.expect("run");
    assert_eq!(
        outcome.output["nodes"]["recover"]["items"][0]["json"]["error"]["node"],
        json!("x"),
        "recovery node must receive the routed error item"
    );
    assert!(
        outcome.output["nodes"]["normal"].is_null(),
        "the main branch must not run when the error routes to `error`"
    );
}
