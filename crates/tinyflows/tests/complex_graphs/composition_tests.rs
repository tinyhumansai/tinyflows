/// scatter → per-lane child workflow → gather → accumulator loop → scatter.
///
/// This crosses both state namespaces (lane slots and child `nodes`) before a
/// repeated reducer write and then fans the accumulated result out again.
#[tokio::test]
async fn scatter_child_gather_loop_and_second_scatter_compose() {
    let child = serde_json::to_value(child_transform()).expect("serialize child");
    let graph = WorkflowGraph {
        name: "two_stage_refinement".to_string(),
        nodes: vec![
            node(
                "trigger",
                NodeKind::Trigger,
                json!({ "recursion_limit": 500, "max_node_visits": 200, "max_concurrency": 3 }),
            ),
            node(
                "first_scatter",
                NodeKind::Scatter,
                json!({ "path": "rows" }),
            ),
            node("child", NodeKind::SubWorkflow, json!({ "workflow": child })),
            node(
                "first_gather",
                NodeKind::Gather,
                json!({ "from": ["child"], "release": "quorum", "n": 3, "poll_interval_ms": 1 }),
            ),
            node(
                "refine_loop",
                NodeKind::Loop,
                json!({
                    "max_iterations": 2,
                    "on_exceeded": "continue",
                    "emit": "both",
                    "state": {
                        "init": { "passes": 0 },
                        "update": { "passes": "=state.passes + 1" },
                    },
                }),
            ),
            node(
                "loop_body",
                NodeKind::Transform,
                json!({ "set": { "refined": true } }),
            ),
            node("second_scatter", NodeKind::Scatter, Value::Null),
            node(
                "finalize",
                NodeKind::Transform,
                json!({ "set": { "final": true } }),
            ),
            node(
                "second_gather",
                NodeKind::Gather,
                json!({ "from": ["finalize"], "poll_interval_ms": 1 }),
            ),
        ],
        edges: vec![
            edge("trigger", "first_scatter"),
            edge("first_scatter", "child"),
            edge("child", "first_gather"),
            edge("first_gather", "refine_loop"),
            port_edge("refine_loop", "body", "loop_body"),
            edge("loop_body", "refine_loop"),
            port_edge("refine_loop", "done", "second_scatter"),
            edge("second_scatter", "finalize"),
            edge("finalize", "second_gather"),
        ],
        ..Default::default()
    };

    let compiled = compile(&graph).expect("complex graph compiles");
    let trace = Arc::new(Trace::default());
    let observer: Arc<dyn RunObserver> = trace.clone();
    let outcome = tokio::time::timeout(
        GUARD,
        run_with_observer(
            &compiled,
            json!({ "rows": [{"id": 0}, {"id": 1}, {"id": 2}] }),
            &mock_capabilities(),
            &observer,
        ),
    )
    .await
    .expect("complex graph hung")
    .expect("complex graph runs");

    assert_eq!(
        outcome.output["nodes"]["child"]["lanes"]
            .as_object()
            .map(serde_json::Map::len),
        Some(3),
        "one child workflow ran in each first-stage lane"
    );
    assert_eq!(outcome.output["nodes"]["refine_loop"]["iteration"], 2);
    assert_eq!(
        outcome.output["nodes"]["refine_loop"]["state"],
        json!({ "passes": 2 }),
        "the accumulator was replaced cleanly on both passes"
    );
    let final_items = outcome.output["nodes"]["second_gather"]["items"]
        .as_array()
        .expect("second gather output");
    assert_eq!(final_items.len(), 4, "three child results plus accumulator");
    assert!(
        final_items.iter().all(|item| item["json"]["final"] == true),
        "every second-stage lane ran the finalizer"
    );

    let order = trace.0.lock().expect("trace mutex poisoned").clone();
    let first_gather = order.iter().position(|id| id == "first_gather").unwrap();
    let second_scatter = order.iter().position(|id| id == "second_scatter").unwrap();
    assert!(first_gather < second_scatter, "observed order: {order:?}");
    assert_eq!(
        order.iter().filter(|id| id.as_str() == "loop_body").count(),
        2,
        "the loop body must activate once per pass: {order:?}"
    );
}
/// A child starts and collects asynchronous work, then pauses for approval;
/// the resumed parent starts and collects a second asynchronous task.
#[tokio::test]
async fn nested_async_gate_and_approval_resume_across_a_subworkflow_boundary() {
    let child = WorkflowGraph {
        name: "async_child".to_string(),
        nodes: vec![
            node("ct", NodeKind::Trigger, Value::Null),
            node(
                "cspawn",
                NodeKind::Spawn,
                json!({ "target": "tool", "slug": "child.lookup" }),
            ),
            node(
                "cgate",
                NodeKind::Gate,
                json!({ "from": ["cspawn"], "poll_interval_ms": 1 }),
            ),
            node(
                "approve",
                NodeKind::OutputParser,
                json!({ "requires_approval": true }),
            ),
            node("cdone", NodeKind::OutputParser, Value::Null),
        ],
        edges: vec![
            edge("ct", "cspawn"),
            edge("cspawn", "cgate"),
            edge("cgate", "approve"),
            edge("approve", "cdone"),
        ],
        ..Default::default()
    };
    let graph = WorkflowGraph {
        name: "nested_async_approval".to_string(),
        nodes: vec![
            node("t", NodeKind::Trigger, json!({ "recursion_limit": 100 })),
            node(
                "sub",
                NodeKind::SubWorkflow,
                json!({ "workflow": serde_json::to_value(child).unwrap() }),
            ),
            node(
                "pspawn",
                NodeKind::Spawn,
                json!({ "target": "tool", "slug": "parent.publish" }),
            ),
            node(
                "pgate",
                NodeKind::Gate,
                json!({ "from": ["pspawn"], "poll_interval_ms": 1 }),
            ),
        ],
        edges: vec![
            edge("t", "sub"),
            edge("sub", "pspawn"),
            edge("pspawn", "pgate"),
        ],
        ..Default::default()
    };

    let compiled = compile(&graph).expect("compile");
    let caps = mock_capabilities();
    let resumable = tokio::time::timeout(GUARD, run_resumable(&compiled, json!({}), &caps))
        .await
        .expect("initial run hung")
        .expect("initial run");
    assert_eq!(
        resumable.outcome().pending_approvals,
        vec!["sub::approve".to_string()]
    );
    assert!(resumable.outcome().output["nodes"]["pspawn"].is_null());

    let done = tokio::time::timeout(GUARD, resumable.resume(vec!["sub::approve".to_string()]))
        .await
        .expect("resume hung")
        .expect("resume");
    assert!(done.pending_approvals.is_empty());
    assert_eq!(done.output["nodes"]["pgate"]["arrived"], 1);
    assert_eq!(
        done.output["nodes"]["pgate"]["items"][0]["json"]["slug"],
        "parent.publish"
    );
}
