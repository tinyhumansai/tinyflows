

#[tokio::test]
async fn per_item_runs_the_child_graph_once_per_input_item() {
    // The multiplier: three items in, three complete child runs out.
    let caps = mock_capabilities_with_resolver(
        MockWorkflowResolver::default().with("child-1", passthrough_child()),
    );
    let input = vec![
        crate::data::Item::new(json!({ "topic": "a" })),
        crate::data::Item::new(json!({ "topic": "b" })),
        crate::data::Item::new(json!({ "topic": "c" })),
    ];
    let out = execute_over(
        json!({ "workflow_id": "child-1", "execution": "per_item", "concurrency": 3 }),
        input,
        &caps,
    )
    .await;

    assert_eq!(out.items.len(), 3, "one child run per input item");
    for (index, item) in out.items.iter().enumerate() {
        assert_eq!(item.paired_item, Some(index), "output pairs to its input");
    }
    // Each child was seeded with ONLY its own item: its trigger payload is a
    // one-element item array, not the parent's whole input.
    for item in &out.items {
        assert_eq!(
            item.json["run"]["trigger"]
                .as_array()
                .expect("trigger items")
                .len(),
            1,
            "each child sees exactly its own item"
        );
    }
    let topics: Vec<&str> = out
        .items
        .iter()
        .map(|i| {
            i.json["run"]["trigger"][0]["json"]["topic"]
                .as_str()
                .expect("topic")
        })
        .collect();
    assert_eq!(topics, ["a", "b", "c"], "children keep input order");
}

#[tokio::test]
async fn once_is_still_the_default_and_seeds_the_whole_input_array() {
    // Back-compat: without `execution` the node runs a single child seeded
    // with every input item, exactly as before fan-out existed.
    let caps = mock_capabilities_with_resolver(
        MockWorkflowResolver::default().with("child-1", passthrough_child()),
    );
    let input = vec![
        crate::data::Item::new(json!({ "topic": "a" })),
        crate::data::Item::new(json!({ "topic": "b" })),
    ];
    let out = execute_over(json!({ "workflow_id": "child-1" }), input, &caps).await;

    assert_eq!(
        out.items.len(),
        1,
        "one child run regardless of input count"
    );
    let seeded = &out.items[0].json["run"]["trigger"];
    assert_eq!(
        seeded.as_array().expect("trigger items").len(),
        2,
        "the single child is seeded with the whole input array"
    );
}

#[tokio::test]
async fn per_item_resolves_workflow_id_against_the_current_item() {
    // `=item.x` in `workflow_id` addresses the element this child run is
    // for, so one node can dispatch each item to a different child graph.
    let mut alpha = passthrough_child();
    alpha.name = "alpha".to_string();
    let mut beta = passthrough_child();
    beta.name = "beta".to_string();
    let caps = mock_capabilities_with_resolver(
        MockWorkflowResolver::default()
            .with("wf-alpha", alpha)
            .with("wf-beta", beta),
    );
    let input = vec![
        crate::data::Item::new(json!({ "which": "wf-alpha" })),
        crate::data::Item::new(json!({ "which": "wf-beta" })),
    ];
    let out = execute_over(
        json!({ "workflow_id": "=item.which", "execution": "per_item" }),
        input,
        &caps,
    )
    .await;
    assert_eq!(out.items.len(), 2);
    // Both resolved (an unknown id would have errored the batch), and each
    // child echoed its own seed.
    assert_eq!(
        out.items[0].json["run"]["trigger"][0]["json"]["which"],
        "wf-alpha"
    );
    assert_eq!(
        out.items[1].json["run"]["trigger"][0]["json"]["which"],
        "wf-beta"
    );
}

#[tokio::test]
async fn a_fanned_out_child_failure_is_collected_not_fatal() {
    // Only `wf-ok` resolves; the other item's child fails to resolve. Under
    // a fan-out's collect default the batch still returns one item per
    // input, with the failure marked for a downstream branch.
    let caps = mock_capabilities_with_resolver(
        MockWorkflowResolver::default().with("wf-ok", passthrough_child()),
    );
    let input = vec![
        crate::data::Item::new(json!({ "which": "wf-ok" })),
        crate::data::Item::new(json!({ "which": "wf-missing" })),
    ];
    let out = execute_over(
        json!({ "workflow_id": "=item.which", "execution": "per_item", "concurrency": 2 }),
        input,
        &caps,
    )
    .await;

    assert_eq!(out.items.len(), 2, "one output per input even on failure");
    assert!(
        out.items[0].json["nodes"]["ct"].is_object(),
        "the good child ran"
    );
    assert_eq!(out.items[1].json["json"]["failed"], true);
    assert!(
        out.items[1].json["json"]["error"]
            .as_str()
            .expect("error message")
            .contains("wf-missing")
    );
}

#[tokio::test]
async fn a_fan_out_widens_the_run_without_deepening_it() {
    // Every sibling child runs at depth+1; a fan-out of N must not consume N
    // levels of the nesting budget (which would make wide fan-outs of nested
    // workflows spuriously trip the cycle guard).
    let caps = mock_capabilities_with_resolver(
        MockWorkflowResolver::default().with("child-1", passthrough_child()),
    );
    let input: Vec<_> = (0..12)
        .map(|i| crate::data::Item::new(json!({ "i": i })))
        .collect();
    let out = execute_over(
        json!({ "workflow_id": "child-1", "execution": "per_item", "concurrency": "all" }),
        input,
        &caps,
    )
    .await;
    assert_eq!(out.items.len(), 12, "12 siblings all completed");
    for item in &out.items {
        assert_eq!(
            item.json["run"]["sub_workflow_depth"], 1,
            "every sibling runs one level down, not cumulatively deeper"
        );
    }
}

#[tokio::test]
async fn missing_workflow_config_is_a_capability_error() {
    let err = execute_err(Value::Null).await;
    assert!(
        matches!(err, EngineError::Capability(ref m) if m.contains("workflow")),
        "expected a capability error mentioning `workflow`, got: {err:?}"
    );
}

#[tokio::test]
async fn invalid_workflow_value_is_a_capability_error() {
    // A non-graph value under `workflow` fails to deserialize into a graph.
    let err = execute_err(json!({ "workflow": 123 })).await;
    assert!(
        matches!(err, EngineError::Capability(ref m) if m.contains("invalid workflow")),
        "expected a capability error about an invalid workflow, got: {err:?}"
    );
}

#[tokio::test]
async fn sub_workflow_runs_embedded_child_graph() {
    // The child is a single trigger node; serialize it into the parent's
    // sub_workflow config so the executor compiles and runs it.
    let child = WorkflowGraph {
        nodes: vec![node("ct", NodeKind::Trigger)],
        ..Default::default()
    };
    let child_value = serde_json::to_value(&child).expect("serialize child");

    let mut sw = node("sw", NodeKind::SubWorkflow);
    sw.config = json!({ "workflow": child_value });

    let parent = WorkflowGraph {
        nodes: vec![node("t", NodeKind::Trigger), sw],
        edges: vec![Edge {
            from_node: "t".to_string(),
            from_port: "main".to_string(),
            to_node: "sw".to_string(),
            to_port: "main".to_string(),
        }],
        ..Default::default()
    };

    let compiled = compile(&parent).expect("compile parent");
    let caps = mock_capabilities();

    let out = run(&compiled, json!({ "hi": 1 }), &caps)
        .await
        .expect("run parent");

    // The sub_workflow emits the child's final run state as its single item.
    // The child seeds its trigger from the input the parent passed, which is
    // the serialized parent items delivered to the sub_workflow node — an
    // array of `Item`s — so the child's `run.trigger` is that array.
    let child_state = &out.output["nodes"]["sw"]["items"][0]["json"];
    assert_eq!(
        child_state["run"]["trigger"],
        json!([{ "json": { "hi": 1 } }]),
        "child trigger should be seeded with the parent's serialized items"
    );
    // And the child actually ran: its trigger node recorded that same payload.
    assert_eq!(
        child_state["nodes"]["ct"]["items"][0]["json"],
        json!([{ "json": { "hi": 1 } }]),
        "child trigger node should have run and echoed its seeded input"
    );
}

/// Executes a lone `sub_workflow` node with the given config, run metadata,
/// and capabilities, returning its raw [`Result`].
async fn execute_with(
    config: Value,
    run_meta: Value,
    caps: &Capabilities,
) -> Result<NodeOutput, EngineError> {
    let mut sw = node("sw", NodeKind::SubWorkflow);
    sw.config = config;
    let input = vec![];
    let ctx = NodeContext {
        node: &sw,
        input: &input,
        run: &run_meta,
        nodes: &Value::Null,
        caps,
        agents: &[],
        observer: &crate::observability::NoopObserver,
        token: crate::engine::CancellationToken::new(),
        lane: None,
        resume: None,
        step: 0,
    };
    SubWorkflowNode.execute(ctx).await
}

#[tokio::test]
async fn both_workflow_and_workflow_id_is_rejected() {
    // Exactly one of the two config keys may be set.
    let err = execute_err(json!({
        "workflow": { "nodes": [], "edges": [] },
        "workflow_id": "child-1"
    }))
    .await;
    assert!(
        matches!(err, EngineError::Capability(ref m) if m.contains("exactly one")),
        "expected an exactly-one config error, got: {err:?}"
    );
}

#[tokio::test]
async fn empty_workflow_id_falls_back_to_missing_config_error() {
    // A blank `workflow_id` is treated as absent, so with no inline
    // `workflow` either the node reports the missing-config error.
    let err = execute_err(json!({ "workflow_id": "" })).await;
    assert!(
        matches!(err, EngineError::Capability(ref m) if m.contains("missing")),
        "expected a missing-config error, got: {err:?}"
    );
}

#[tokio::test]
async fn sub_workflow_by_id_resolves_via_resolver_and_executes() {
    // The saved child is a single trigger node, registered under an id the
    // parent references via `workflow_id`.
    let child = WorkflowGraph {
        nodes: vec![node("ct", NodeKind::Trigger)],
        ..Default::default()
    };
    let caps =
        mock_capabilities_with_resolver(MockWorkflowResolver::default().with("child-1", child));

    let mut sw = node("sw", NodeKind::SubWorkflow);
    sw.config = json!({ "workflow_id": "child-1" });
    let parent = WorkflowGraph {
        nodes: vec![node("t", NodeKind::Trigger), sw],
        edges: vec![Edge {
            from_node: "t".to_string(),
            from_port: "main".to_string(),
            to_node: "sw".to_string(),
            to_port: "main".to_string(),
        }],
        ..Default::default()
    };
    let compiled = compile(&parent).expect("compile parent");

    let out = run(&compiled, json!({ "hi": 1 }), &caps)
        .await
        .expect("run parent");

    // The referenced child was resolved and actually ran.
    let child_state = &out.output["nodes"]["sw"]["items"][0]["json"];
    assert_eq!(
        child_state["nodes"]["ct"]["items"][0]["json"],
        json!([{ "json": { "hi": 1 } }]),
        "resolved child trigger node should have run and echoed its seeded input"
    );
    // The child ran one nesting level deep.
    assert_eq!(child_state["run"]["sub_workflow_depth"], json!(1));
}

#[tokio::test]
async fn unknown_workflow_id_surfaces_resolver_error() {
    // The default mock resolver knows no ids, so resolution fails.
    let caps = mock_capabilities();
    let err = execute_with(json!({ "workflow_id": "nope" }), Value::Null, &caps)
        .await
        .expect_err("unknown id must error");
    assert!(
        matches!(err, EngineError::Capability(ref m) if m.contains("nope")),
        "expected the resolver's unknown-id error, got: {err:?}"
    );
}

#[tokio::test]
async fn direct_self_reference_by_id_is_rejected() {
    // The saved child itself references the same id — a one-level cycle,
    // caught statically before it runs.
    let mut inner = node("inner", NodeKind::SubWorkflow);
    inner.config = json!({ "workflow_id": "loop-1" });
    let child = WorkflowGraph {
        nodes: vec![node("ct", NodeKind::Trigger), inner],
        ..Default::default()
    };
    let caps =
        mock_capabilities_with_resolver(MockWorkflowResolver::default().with("loop-1", child));

    let err = execute_with(json!({ "workflow_id": "loop-1" }), Value::Null, &caps)
        .await
        .expect_err("self-reference must be rejected");
    assert!(
        matches!(err, EngineError::Capability(ref m) if m.contains("cycle")),
        "expected a cycle rejection, got: {err:?}"
    );
}

#[tokio::test]
async fn depth_limit_is_enforced() {
    // A run already at the maximum nesting depth refuses to descend further,
    // even for a trivial resolvable child (bounds indirect cycles).
    let child = WorkflowGraph {
        nodes: vec![node("ct", NodeKind::Trigger)],
        ..Default::default()
    };
    let caps =
        mock_capabilities_with_resolver(MockWorkflowResolver::default().with("child-1", child));

    let run_meta = json!({ "sub_workflow_depth": crate::engine::MAX_SUB_WORKFLOW_DEPTH });
    let err = execute_with(json!({ "workflow_id": "child-1" }), run_meta, &caps)
        .await
        .expect_err("exceeding the depth budget must error");
    assert!(
        matches!(err, EngineError::Capability(ref m) if m.contains("depth")),
        "expected a depth-limit error, got: {err:?}"
    );
}

/// A run carrying `max_sub_workflow_depth` uses it in place of the default,
/// so a graph that legitimately nests deeper is not capped at 8.
#[tokio::test]
async fn a_run_declared_depth_raises_the_default_cap() {
    let child = WorkflowGraph {
        nodes: vec![node("ct", NodeKind::Trigger)],
        ..Default::default()
    };
    let caps =
        mock_capabilities_with_resolver(MockWorkflowResolver::default().with("child-1", child));

    // At the default cap, but the run declared a higher one — so this
    // descends rather than refusing.
    let run_meta = json!({
        "sub_workflow_depth": crate::engine::MAX_SUB_WORKFLOW_DEPTH,
        "max_sub_workflow_depth": crate::engine::MAX_SUB_WORKFLOW_DEPTH + 4,
    });
    execute_with(json!({ "workflow_id": "child-1" }), run_meta, &caps)
        .await
        .expect("a raised cap should permit this level");
}

/// The declared cap bounds as well as raises: a lower value bites before the
/// default would have.
#[tokio::test]
async fn a_run_declared_depth_can_also_lower_the_cap() {
    let child = WorkflowGraph {
        nodes: vec![node("ct", NodeKind::Trigger)],
        ..Default::default()
    };
    let caps =
        mock_capabilities_with_resolver(MockWorkflowResolver::default().with("child-1", child));

    let run_meta = json!({ "sub_workflow_depth": 2, "max_sub_workflow_depth": 2 });
    let err = execute_with(json!({ "workflow_id": "child-1" }), run_meta, &caps)
        .await
        .expect_err("a lowered cap must be enforced");
    assert!(
        matches!(err, EngineError::Capability(ref m) if m.contains("depth 2")),
        "the error should name the declared cap, got: {err:?}"
    );
}
