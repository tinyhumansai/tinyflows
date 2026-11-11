/// A refinement loop whose body scatters, with `candidates` proposed per pass.
///
/// `candidates` is the expression `propose` sets per pass; each candidate is
/// evaluated in its own lane, gathered, and folded into the loop state.
fn refinement_loop(candidates: &str, loop_config: Value) -> WorkflowGraph {
    WorkflowGraph {
        name: "refinement_loop_scatter".to_string(),
        nodes: vec![
            node(
                "t",
                NodeKind::Trigger,
                json!({ "recursion_limit": 200, "max_node_visits": 100 }),
            ),
            node("refine", NodeKind::Loop, loop_config),
            node(
                "propose",
                NodeKind::Transform,
                json!({ "set": { "candidates": candidates } }),
            ),
            node("scatter", NodeKind::Scatter, json!({ "path": "candidates" })),
            node(
                "evaluate",
                NodeKind::ToolCall,
                json!({ "slug": "refine.evaluate", "args": "=item" }),
            ),
            node(
                "gather",
                NodeKind::Gather,
                json!({ "from": ["evaluate"], "poll_interval_ms": 1 }),
            ),
            node("out", NodeKind::OutputParser, Value::Null),
        ],
        edges: vec![
            edge("t", "refine"),
            port_edge("refine", "body", "propose"),
            edge("propose", "scatter"),
            edge("scatter", "evaluate"),
            edge("evaluate", "gather"),
            edge("gather", "refine"),
            port_edge("refine", "done", "out"),
        ],
        ..Default::default()
    }
}

fn gathered_candidates(output: &Value) -> Vec<i64> {
    output["nodes"]["gather"]["items"]
        .as_array()
        .expect("gather items")
        .iter()
        .filter_map(|item| item["json"]["json"]["c"].as_i64())
        .collect()
}

/// Fan out per candidate, gather, accumulate the best score, stop `until` it
/// passes. Three candidates on every pass: pass k proposes 10k, 10k+1, 10k+2,
/// and `until best >= 30` stops after the third.
#[tokio::test]
async fn a_refinement_loop_scatters_its_candidates_on_every_pass() {
    let graph = refinement_loop(
        "=[{c: (.nodes.refine.iteration * 10)}, {c: (.nodes.refine.iteration * 10 + 1)}, \
         {c: (.nodes.refine.iteration * 10 + 2)}]",
        json!({
            "max_iterations": 5,
            "state": {
                "init": { "best": 0, "passes": 0 },
                "update": {
                    "best": "=[.items[].json.c] | max",
                    "passes": "=state.passes + 1",
                },
            },
            "until": "=state.best >= 30",
        }),
    );
    let compiled = compile(&graph).expect("a scatter inside a loop body is accepted");
    let mut caps = mock_capabilities();
    caps.tools = Arc::new(SelectiveFailure);
    let trace = Arc::new(Trace::default());
    let observer: Arc<dyn RunObserver> = trace.clone();
    let outcome = tokio::time::timeout(
        GUARD,
        run_with_observer(&compiled, json!({}), &caps, &observer),
    )
    .await
    .expect("refinement loop hung")
    .expect("refinement loop runs");
    let output = &outcome.output;

    assert_eq!(output["nodes"]["refine"]["exit_reason"], "until");
    assert_eq!(output["nodes"]["refine"]["iteration"], 3);
    assert_eq!(
        output["nodes"]["refine"]["state"],
        json!({ "best": 32, "passes": 3 })
    );
    assert_eq!(
        gathered_candidates(output),
        vec![30, 31, 32],
        "the last gather holds exactly the last pass's lanes, in lane order"
    );
    assert!(output["nodes"]["evaluate"].get("items").is_none());
    assert!(
        output["nodes"]["out"]["items"].is_array(),
        "the converged loop left through done"
    );

    let mut order = trace.0.lock().expect("trace mutex poisoned").clone();
    order.retain(|id| id != "t");
    let pass = ["propose", "scatter", "evaluate", "evaluate", "evaluate", "gather", "refine"];
    let mut expected = vec!["refine"];
    for _ in 0..3 {
        expected.extend(pass);
    }
    expected.push("out");
    assert_eq!(order, expected, "observed activation order");
}

/// scatter → switch → { fast: tool(on_error route) → ok | recover, slow: tool }
/// → gather. Row 1 fails on the routed arm (recovered as data); row 3 fails on
/// the unhandled arm (a failed lane, left to `on_lane_error`).
fn lane_switch_graph(policy: &str) -> WorkflowGraph {
    WorkflowGraph {
        name: format!("lane_switch_{policy}"),
        nodes: vec![
            node("t", NodeKind::Trigger, json!({ "recursion_limit": 100 })),
            node("scatter", NodeKind::Scatter, json!({ "path": "rows" })),
            node("kind", NodeKind::Switch, json!({ "expression": "=item.kind" })),
            node(
                "fast",
                NodeKind::ToolCall,
                json!({ "slug": "lane.fast", "args": "=item", "on_error": "route" }),
            ),
            node("fast_ok", NodeKind::Transform, json!({ "set": { "arm": "fast" } })),
            node(
                "fast_recover",
                NodeKind::Transform,
                json!({ "set": { "arm": "recovered" } }),
            ),
            node(
                "slow",
                NodeKind::ToolCall,
                json!({ "slug": "lane.slow", "args": "=item" }),
            ),
            node(
                "gather",
                NodeKind::Gather,
                json!({
                    "from": ["fast_ok", "fast_recover", "slow"],
                    "on_lane_error": policy,
                    "poll_interval_ms": 1,
                }),
            ),
        ],
        edges: vec![
            edge("t", "scatter"),
            edge("scatter", "kind"),
            port_edge("kind", "fast", "fast"),
            port_edge("kind", "slow", "slow"),
            port_edge("fast", "main", "fast_ok"),
            port_edge("fast", "error", "fast_recover"),
            edge("fast_ok", "gather"),
            edge("fast_recover", "gather"),
            edge("slow", "gather"),
        ],
        ..Default::default()
    }
}

fn positions(order: &[String], ids: &[&str]) -> Vec<usize> {
    order
        .iter()
        .enumerate()
        .filter(|(_, id)| ids.contains(&id.as_str()))
        .map(|(index, _)| index)
        .collect()
}

/// Super-steps complete in order, so every switch finishes before any arm and
/// every arm tool before its routed follow-up.
fn assert_lane_order(order: &[String]) {
    let count = |id: &str| order.iter().filter(|seen| *seen == id).count();
    assert_eq!(count("kind"), 4, "one switch per lane: {order:?}");
    assert_eq!(count("fast"), 2, "{order:?}");
    assert_eq!(count("slow"), 2, "{order:?}");
    let scatter = positions(order, &["scatter"]);
    let switches = positions(order, &["kind"]);
    let arms = positions(order, &["fast", "slow"]);
    assert!(scatter.iter().max() < switches.iter().min(), "{order:?}");
    assert!(switches.iter().max() < arms.iter().min(), "{order:?}");
    assert!(order.iter().any(|id| id == "gather"), "{order:?}");
}

/// A lane-local `switch` with an `on_error: route` arm, under every
/// `on_lane_error` policy, asserting final state and observer order.
#[tokio::test]
async fn a_lane_switch_with_a_routed_error_arm_under_every_lane_policy() {
    let input = json!({ "rows": [
        { "id": 0, "kind": "fast" },
        { "id": 1, "kind": "fast", "fail": true },
        { "id": 2, "kind": "slow" },
        { "id": 3, "kind": "slow", "fail": true }
    ] });
    let mut caps = mock_capabilities();
    caps.tools = Arc::new(SelectiveFailure);

    for policy in ["collect", "skip", "fail_fast"] {
        let compiled = compile(&lane_switch_graph(policy)).expect("lane switch compiles");
        let trace = Arc::new(Trace::default());
        let observer: Arc<dyn RunObserver> = trace.clone();
        let result = tokio::time::timeout(
            GUARD,
            run_with_observer(&compiled, input.clone(), &caps, &observer),
        )
        .await
        .expect("lane switch hung");
        let order = trace.0.lock().expect("trace mutex poisoned").clone();
        assert_lane_order(&order);

        if policy == "fail_fast" {
            let error = result.expect_err("fail_fast fails the run").to_string();
            assert!(error.contains("fail_fast"), "{error}");
            assert!(error.contains("scheduled lane failure"), "{error}");
            continue;
        }

        let outcome = result.expect("lane switch runs");
        let nodes = &outcome.output["nodes"];
        for id in ["kind", "fast", "fast_ok", "fast_recover", "slow"] {
            assert!(
                nodes[id].get("items").is_none(),
                "{policy}: a lane wrote {id}'s top-level slot"
            );
        }
        assert_eq!(order.last().map(String::as_str), Some("gather"), "{order:?}");
        let fast = positions(&order, &["fast"]);
        let follow_ups = positions(&order, &["fast_ok", "fast_recover"]);
        assert_eq!(follow_ups.len(), 2, "one routed follow-up per fast lane: {order:?}");
        assert!(fast.iter().max() < follow_ups.iter().min(), "{order:?}");

        let items = nodes["gather"]["items"].as_array().expect("gather items");
        assert_eq!(items[0]["json"]["arm"], "fast", "{policy}: lane 0");
        assert_eq!(items[1]["json"]["arm"], "recovered", "{policy}: lane 1");
        assert_eq!(items[2]["json"]["json"]["id"], 2, "{policy}: lane 2");
        match policy {
            "collect" => {
                assert_eq!(items.len(), 4, "{policy}: {items:?}");
                assert_eq!(items[3]["json"]["failed"], true);
                assert_eq!(items[3]["json"]["lane"], 3);
            }
            _ => assert_eq!(items.len(), 3, "{policy} drops the failed lane: {items:?}"),
        }
    }
}
