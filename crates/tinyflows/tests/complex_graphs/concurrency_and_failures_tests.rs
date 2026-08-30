struct ConcurrencyProbe {
    in_flight: AtomicUsize,
    peak: AtomicUsize,
}

#[async_trait::async_trait]
impl ToolInvoker for ConcurrencyProbe {
    async fn invoke(
        &self,
        _slug: &str,
        args: Value,
        _conn: Option<&str>,
    ) -> tinyflows::error::Result<Value> {
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        for _ in 0..6 {
            tokio::task::yield_now().await;
        }
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        Ok(args)
    }
}

/// The maximum graph concurrency also bounds a 256-lane scatter, rather than
/// applying only to ordinary static fan-out branches.
#[tokio::test]
async fn a_wide_scatter_honours_the_global_admission_bound() {
    let graph = WorkflowGraph {
        name: "wide_bounded_scatter".to_string(),
        nodes: vec![
            node(
                "t",
                NodeKind::Trigger,
                json!({ "max_concurrency": 4, "recursion_limit": 500 }),
            ),
            node("scatter", NodeKind::Scatter, json!({ "path": "rows" })),
            node(
                "work",
                NodeKind::ToolCall,
                json!({ "slug": "lane.work", "args": "=item" }),
            ),
            node(
                "gather",
                NodeKind::Gather,
                json!({ "from": ["work"], "poll_interval_ms": 1 }),
            ),
        ],
        edges: vec![
            edge("t", "scatter"),
            edge("scatter", "work"),
            edge("work", "gather"),
        ],
        ..Default::default()
    };
    let probe = Arc::new(ConcurrencyProbe {
        in_flight: AtomicUsize::new(0),
        peak: AtomicUsize::new(0),
    });
    let mut caps = mock_capabilities();
    caps.tools = probe.clone();
    let compiled = compile(&graph).expect("compile");
    let rows: Vec<Value> = (0..256).map(|index| json!({ "index": index })).collect();

    let outcome = tokio::time::timeout(
        GUARD,
        tinyflows::engine::run(&compiled, json!({ "rows": rows }), &caps),
    )
    .await
    .expect("wide scatter hung")
    .expect("wide scatter runs");
    let peak = probe.peak.load(Ordering::SeqCst);
    assert!(peak > 1, "lanes should overlap, observed peak {peak}");
    assert!(peak <= 4, "max_concurrency=4 admitted {peak} lanes at once");
    assert_eq!(
        outcome.output["nodes"]["gather"]["items"]
            .as_array()
            .map(Vec::len),
        Some(256)
    );
}

struct SelectiveFailure;

#[async_trait::async_trait]
impl ToolInvoker for SelectiveFailure {
    async fn invoke(
        &self,
        _slug: &str,
        args: Value,
        _conn: Option<&str>,
    ) -> tinyflows::error::Result<Value> {
        if args.get("fail").and_then(Value::as_bool) == Some(true) {
            Err(tinyflows::error::EngineError::Capability(
                "scheduled lane failure".to_string(),
            ))
        } else {
            Ok(args)
        }
    }
}

fn failing_lane_graph(policy: &str) -> WorkflowGraph {
    WorkflowGraph {
        name: format!("lane_errors_{policy}"),
        nodes: vec![
            node("t", NodeKind::Trigger, json!({ "recursion_limit": 100 })),
            node("scatter", NodeKind::Scatter, json!({ "path": "rows" })),
            node(
                "work",
                NodeKind::ToolCall,
                json!({ "slug": "lane.maybe_fail", "args": "=item" }),
            ),
            node(
                "gather",
                NodeKind::Gather,
                json!({
                    "from": ["work"],
                    "on_lane_error": policy,
                    "poll_interval_ms": 1,
                }),
            ),
        ],
        edges: vec![
            edge("t", "scatter"),
            edge("scatter", "work"),
            edge("work", "gather"),
        ],
        ..Default::default()
    }
}

/// One lane fails while its siblings succeed, under all three gather policies.
#[tokio::test]
async fn lane_failures_collect_skip_or_fail_fast_as_configured() {
    let input = json!({ "rows": [
        { "id": 0 },
        { "id": 1, "fail": true },
        { "id": 2 }
    ] });
    let mut caps = mock_capabilities();
    caps.tools = Arc::new(SelectiveFailure);

    let collect = compile(&failing_lane_graph("collect")).expect("compile collect");
    let collected = tinyflows::engine::run(&collect, input.clone(), &caps)
        .await
        .expect("collect run");
    let items = collected.output["nodes"]["gather"]["items"]
        .as_array()
        .expect("collected items");
    assert_eq!(items.len(), 3);
    assert_eq!(
        items
            .iter()
            .filter(|item| item["json"]["failed"] == true)
            .count(),
        1
    );

    let skip = compile(&failing_lane_graph("skip")).expect("compile skip");
    let skipped = tinyflows::engine::run(&skip, input.clone(), &caps)
        .await
        .expect("skip run");
    assert_eq!(
        skipped.output["nodes"]["gather"]["items"]
            .as_array()
            .map(Vec::len),
        Some(2)
    );

    let fail_fast = compile(&failing_lane_graph("fail_fast")).expect("compile fail_fast");
    let error = tinyflows::engine::run(&fail_fast, input, &caps)
        .await
        .expect_err("fail_fast must fail the run");
    assert!(error.to_string().contains("scheduled lane failure"));
}

/// Handled lane errors stay in lane-local state for both recovery policies.
/// The routed form also proves that different lanes can take different ports
/// and still reconverge at one gather.
#[tokio::test]
async fn lane_error_continue_and_route_never_write_the_top_level_slot() {
    let input = json!({ "rows": [
        { "id": 0 },
        { "id": 1, "fail": true },
        { "id": 2 }
    ] });
    let mut caps = mock_capabilities();
    caps.tools = Arc::new(SelectiveFailure);

    let mut continued = failing_lane_graph("collect");
    continued.name = "lane_continue".to_string();
    continued
        .nodes
        .iter_mut()
        .find(|node| node.id == "work")
        .expect("work node")
        .config["on_error"] = json!("continue");
    let outcome = tinyflows::engine::run(
        &compile(&continued).expect("compile continue"),
        input.clone(),
        &caps,
    )
    .await
    .expect("continue run");
    assert!(outcome.output["nodes"]["work"].get("items").is_none());
    assert_eq!(
        outcome.output["nodes"]["gather"]["items"]
            .as_array()
            .map(Vec::len),
        Some(3)
    );

    let routed = WorkflowGraph {
        name: "lane_error_route".to_string(),
        nodes: vec![
            node("t", NodeKind::Trigger, json!({ "recursion_limit": 100 })),
            node("scatter", NodeKind::Scatter, json!({ "path": "rows" })),
            node(
                "work",
                NodeKind::ToolCall,
                json!({
                    "slug": "lane.maybe_fail",
                    "args": "=item",
                    "on_error": "route",
                }),
            ),
            node(
                "success",
                NodeKind::Transform,
                json!({ "set": { "route": "main" } }),
            ),
            node(
                "recover",
                NodeKind::Transform,
                json!({ "set": { "route": "error" } }),
            ),
            node(
                "gather",
                NodeKind::Gather,
                json!({ "from": ["success", "recover"], "poll_interval_ms": 1 }),
            ),
        ],
        edges: vec![
            edge("t", "scatter"),
            edge("scatter", "work"),
            port_edge("work", "main", "success"),
            port_edge("work", "error", "recover"),
            edge("success", "gather"),
            edge("recover", "gather"),
        ],
        ..Default::default()
    };
    let outcome = tinyflows::engine::run(&compile(&routed).expect("compile route"), input, &caps)
        .await
        .expect("route run");
    assert!(outcome.output["nodes"]["work"].get("items").is_none());
    let items = outcome.output["nodes"]["gather"]["items"]
        .as_array()
        .expect("gather items");
    assert_eq!(items.len(), 3);
    assert_eq!(
        items
            .iter()
            .filter(|item| item["json"]["route"] == "error")
            .count(),
        1,
        "only the failing lane takes the error arm"
    );
}
