

#[test]
fn default_route_update_explicitly_clears_a_previous_port() {
    let update = items_update("worker", &[], None).expect("serialize update");
    assert_eq!(update["nodes"]["worker"]["port"], Value::Null);
}

#[test]
fn loop_reentry_ignores_stale_alternate_return_slots() {
    let state = json!({
        "nodes": {
            "old_arm": {
                "items": [{ "json": { "arm": "old" } }],
                "port": null,
                "_activation_step": 4
            },
            "current_arm": {
                "items": [{ "json": { "arm": "current" } }],
                "port": null,
                "_activation_step": 8
            }
        }
    });
    let incoming = vec![
        ("old_arm".to_string(), "main".to_string()),
        ("current_arm".to_string(), "main".to_string()),
    ];

    let items = collect_input_since(&state, &incoming, Some(8));

    assert_eq!(items.len(), 1);
    assert_eq!(items[0].json["arm"], "current");
}

#[tokio::test]
async fn trigger_only_workflow_runs_end_to_end() {
    let graph = WorkflowGraph {
        nodes: vec![node("t", NodeKind::Trigger)],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let caps = mock_capabilities();

    let outcome = run(&compiled, json!({ "hello": "world" }), &caps)
        .await
        .expect("run");

    assert_eq!(
        outcome.output["run"]["trigger"],
        json!({ "hello": "world" })
    );
    assert_eq!(
        outcome.output["nodes"]["t"]["items"][0]["json"],
        json!({ "hello": "world" })
    );
}

#[tokio::test]
async fn linear_edge_drives_downstream_node() {
    // trigger -> output_parser (a passthrough): proves edge lowering + dispatch
    // by checking the trigger items flow through to the downstream node.
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

    let outcome = run(&compiled, json!({ "x": 1 }), &caps).await.expect("run");
    assert_eq!(
        outcome.output["nodes"]["p"]["items"][0]["json"],
        json!({ "x": 1 })
    );
}

#[tokio::test]
async fn journaled_run_records_graph_observations() {
    // trigger -> output_parser under run_with_checkpointer_journaled: the
    // injected in-memory journal must hold this run's durable
    // GraphObservations (node started/completed) under the graph run
    // id returned on the JournaledRunOutcome.
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
    let checkpointer: Arc<dyn Checkpointer<Value>> =
        Arc::new(InMemoryCheckpointer::<Value>::default());
    let journal = Arc::new(InMemoryGraphEventJournal::new());

    let journaled = run_with_checkpointer_journaled(
        &compiled,
        json!({ "x": 1 }),
        &caps,
        checkpointer,
        "thread-journal-1",
        journal.clone(),
    )
    .await
    .expect("journaled run");

    // The workflow outcome is unchanged from the plain checkpointed path.
    assert_eq!(
        journaled.outcome.output["nodes"]["p"]["items"][0]["json"],
        json!({ "x": 1 })
    );
    assert!(journaled.outcome.pending_approvals.is_empty());

    // The returned run id is the journal's stream key: reading it back
    // replays the run's durable observations.
    let run_id = &journaled.graph_run_ids.run_id;
    assert!(!run_id.is_empty(), "run id must be surfaced");
    assert_eq!(
        journaled.graph_run_ids.root_run_id, *run_id,
        "top-level run: root run id equals run id"
    );
    let observations = journal.read_from(run_id, 0).await.expect("read journal");
    assert!(
        !observations.is_empty(),
        "journal must hold observations for run {run_id}"
    );

    let kinds: Vec<&str> = observations.iter().map(|o| o.event.kind()).collect();
    // Both graph nodes ran: their handler start/completion events are
    // journaled, alongside the run lifecycle.
    assert!(kinds.contains(&"run.started"), "kinds: {kinds:?}");
    assert!(kinds.contains(&"node.started"), "kinds: {kinds:?}");
    assert!(kinds.contains(&"node.completed"), "kinds: {kinds:?}");
    assert!(kinds.contains(&"run.completed"), "kinds: {kinds:?}");
    // Every observation is keyed by the surfaced run id and stamped with
    // the caller's thread id.
    for obs in &observations {
        assert_eq!(obs.run_id.as_str(), run_id);
        assert_eq!(
            obs.thread_id.as_ref().map(|t| t.as_str()),
            Some("thread-journal-1")
        );
    }
}

#[tokio::test]
async fn condition_routes_only_the_taken_branch() {
    // trigger -> condition(field=active) branches to pass_a (true) / pass_b
    // (false), both passthroughs. A truthy input must run only the true branch.
    let mut condition = node("c", NodeKind::Condition);
    condition.config = json!({ "field": "active" });
    let graph = WorkflowGraph {
        nodes: vec![
            node("t", NodeKind::Trigger),
            condition,
            node("pass_a", NodeKind::OutputParser),
            node("pass_b", NodeKind::OutputParser),
        ],
        edges: vec![
            Edge {
                from_node: "t".to_string(),
                from_port: "main".to_string(),
                to_node: "c".to_string(),
                to_port: "main".to_string(),
            },
            Edge {
                from_node: "c".to_string(),
                from_port: "true".to_string(),
                to_node: "pass_a".to_string(),
                to_port: "main".to_string(),
            },
            Edge {
                from_node: "c".to_string(),
                from_port: "false".to_string(),
                to_node: "pass_b".to_string(),
                to_port: "main".to_string(),
            },
        ],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let caps = mock_capabilities();

    let outcome = run(&compiled, json!({ "active": true }), &caps)
        .await
        .expect("run");
    assert!(
        !outcome.output["nodes"]["pass_a"]["items"].is_null(),
        "true branch should have run"
    );
    assert!(
        outcome.output["nodes"]["pass_b"].is_null(),
        "false branch should not have run"
    );
}

#[tokio::test]
async fn fan_out_runs_both_branches() {
    // trigger fans out on port `main` to two independent successors; both must
    // run concurrently (previously this shape was rejected as unimplemented).
    let graph = WorkflowGraph {
        nodes: vec![
            node("t", NodeKind::Trigger),
            node("a", NodeKind::Transform),
            node("b", NodeKind::Transform),
        ],
        edges: vec![
            Edge {
                from_node: "t".to_string(),
                from_port: "main".to_string(),
                to_node: "a".to_string(),
                to_port: "main".to_string(),
            },
            Edge {
                from_node: "t".to_string(),
                from_port: "main".to_string(),
                to_node: "b".to_string(),
                to_port: "main".to_string(),
            },
        ],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let caps = mock_capabilities();

    let outcome = run(&compiled, json!({ "v": 1 }), &caps).await.expect("run");
    assert!(
        !outcome.output["nodes"]["a"]["items"].is_null(),
        "fan-out branch a should have run"
    );
    assert!(
        !outcome.output["nodes"]["b"]["items"].is_null(),
        "fan-out branch b should have run"
    );
}

#[tokio::test]
async fn diamond_fan_out_and_merge() {
    // trigger -> dispatch, which fans out on port `main` to `a` and `b`; both
    // feed a `merge` barrier `m`, then `m -> done`. The barrier must hold until
    // both branches complete, and merge concatenates their items.
    let edge = |from: &str, port: &str, to: &str| Edge {
        from_node: from.to_string(),
        from_port: port.to_string(),
        to_node: to.to_string(),
        to_port: "main".to_string(),
    };
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
            edge("t", "main", "d"),
            edge("d", "main", "a"),
            edge("d", "main", "b"),
            edge("a", "main", "m"),
            edge("b", "main", "m"),
            edge("m", "main", "done"),
        ],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let caps = mock_capabilities();

    let outcome = run(&compiled, json!({ "v": 1 }), &caps).await.expect("run");

    assert!(
        !outcome.output["nodes"]["a"]["items"].is_null(),
        "fan-out branch a should have run"
    );
    assert!(
        !outcome.output["nodes"]["b"]["items"].is_null(),
        "fan-out branch b should have run"
    );
    let merged = outcome.output["nodes"]["m"]["items"]
        .as_array()
        .expect("merge should have produced items");
    assert!(
        merged.len() >= 2,
        "merge should concatenate both branches' items, got {}",
        merged.len()
    );
    assert!(
        !outcome.output["nodes"]["done"]["items"].is_null(),
        "the node past the merge barrier should have run"
    );
}

#[tokio::test]
async fn on_error_continue_emits_error_item() {
    // A `tool_call` with no `slug` deterministically errors; `on_error:
    // continue` turns that into an error item on the default port so the run
    // still completes.
    let mut tool = node("x", NodeKind::ToolCall);
    tool.config = json!({ "on_error": "continue" });
    let graph = WorkflowGraph {
        nodes: vec![node("t", NodeKind::Trigger), tool],
        edges: vec![Edge {
            from_node: "t".to_string(),
            from_port: "main".to_string(),
            to_node: "x".to_string(),
            to_port: "main".to_string(),
        }],
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
async fn on_error_route_sends_error_item_to_error_port() {
    // `on_error: route` emits the error item on the `error` port; an edge from
    // that port must carry it into the downstream handler.
    let mut tool = node("x", NodeKind::ToolCall);
    tool.config = json!({ "on_error": "route" });
    let graph = WorkflowGraph {
        nodes: vec![
            node("t", NodeKind::Trigger),
            tool,
            node("h", NodeKind::OutputParser),
        ],
        edges: vec![
            Edge {
                from_node: "t".to_string(),
                from_port: "main".to_string(),
                to_node: "x".to_string(),
                to_port: "main".to_string(),
            },
            Edge {
                from_node: "x".to_string(),
                from_port: "error".to_string(),
                to_node: "h".to_string(),
                to_port: "main".to_string(),
            },
        ],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let caps = mock_capabilities();

    let outcome = run(&compiled, json!({}), &caps).await.expect("run");
    assert!(
        !outcome.output["nodes"]["h"]["items"][0]["json"]["error"].is_null(),
        "handler should have received the routed error item"
    );
}

#[tokio::test]
async fn on_error_stop_is_default() {
    // No `on_error` config: the tool_call's error must fail the whole run.
    let graph = WorkflowGraph {
        nodes: vec![node("t", NodeKind::Trigger), node("x", NodeKind::ToolCall)],
        edges: vec![Edge {
            from_node: "t".to_string(),
            from_port: "main".to_string(),
            to_node: "x".to_string(),
            to_port: "main".to_string(),
        }],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let caps = mock_capabilities();

    assert!(run(&compiled, json!({}), &caps).await.is_err());
}

#[tokio::test]
async fn retry_then_continue_completes() {
    // Retries are exhausted (the tool_call errors every attempt), then
    // `on_error: continue` yields an error item and the run completes.
    let mut tool = node("x", NodeKind::ToolCall);
    tool.config = json!({ "retry": { "max_attempts": 3 }, "on_error": "continue" });
    let graph = WorkflowGraph {
        nodes: vec![node("t", NodeKind::Trigger), tool],
        edges: vec![Edge {
            from_node: "t".to_string(),
            from_port: "main".to_string(),
            to_node: "x".to_string(),
            to_port: "main".to_string(),
        }],
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
async fn retry_backoff_runs_without_hanging() {
    // trigger -> tool_call with no slug (deterministic error) and an
    // exponential backoff of 1ms across 2 attempts. The tiny delay proves the
    // backoff path executes between attempts without hanging, and `on_error:
    // continue` lets the run complete with an error item. (Actual timeout/limit
    // firing is enforced and tested by the runtime's own tests.)
    let mut tool = node("x", NodeKind::ToolCall);
    tool.config = json!({
        "retry": { "max_attempts": 2, "backoff_ms": 1, "backoff": "exponential" },
        "on_error": "continue"
    });
    let graph = WorkflowGraph {
        nodes: vec![node("t", NodeKind::Trigger), tool],
        edges: vec![Edge {
            from_node: "t".to_string(),
            from_port: "main".to_string(),
            to_node: "x".to_string(),
            to_port: "main".to_string(),
        }],
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
