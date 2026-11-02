use super::*;
use tinyflows_drivers::catalog::FlowCatalogDocuments;

#[tokio::test]
#[ignore = "run through scripts/verify-module-artifact.sh"]
async fn start_run_reports_the_missing_declared_input_at_the_bus_boundary() {
    let (_module_host, connection, catalog_proxy, runs_proxy) =
        super::run_lifecycle_tests::module_proxies().await;
    let owner = owner(&connection, "missing-repo", None, None, None, None).await;
    let graph = serde_json::json!({
        "inputs": [{"name":"repo", "type":"string", "required":true}],
        "nodes": [{"id":"trigger", "name":"Trigger", "kind":"trigger", "config":{}}],
        "edges": []
    });
    let flow_id = create_flow_with_graph(
        &catalog_proxy,
        &owner.storage,
        "Missing repo",
        graph.clone(),
    )
    .await;
    let mut request = request(&owner, flow_id, "missing-repo-run", graph, 20_000);
    request.inputs.clear();

    let error = runs_proxy
        .call::<runs::StartRunResponse>("StartRun", (request,))
        .await
        .expect_err("missing required workflow input must reject StartRun");
    let message = error.to_string();
    assert!(
        message.contains("repo"),
        "input field name was lost: {message}"
    );
    assert!(
        message.contains("required but was not supplied"),
        "actionable validation cause was lost: {message}"
    );

    let row: Option<catalog::FlowRun> = catalog_proxy
        .call(
            "GetRun",
            (RunIdRequest {
                storage: owner.storage,
                id: "missing-repo-run".into(),
            },),
        )
        .await
        .unwrap();
    assert_eq!(row, None, "preflight failure must not create a run row");
}

#[tokio::test]
#[ignore = "run through scripts/verify-module-artifact.sh"]
async fn continue_policy_persists_step_failure_in_the_terminal_event_and_run_row() {
    let (_module_host, connection, catalog_proxy, runs_proxy) =
        super::run_lifecycle_tests::module_proxies().await;
    let owner = owner(&connection, "continue-failure", None, None, None, None).await;
    let mut graph = dedup_graph("continue-agent");
    graph["nodes"][2]["config"]["on_error"] = serde_json::json!("continue");
    let flow_id = create_flow_with_graph(
        &catalog_proxy,
        &owner.storage,
        "Continue failure",
        graph.clone(),
    )
    .await;
    let _agent = serve_failing_agent(&connection, "continue-failure").await;
    let mut start = request(&owner, flow_id, "continue-failure-run", graph, 20_000);
    start.trigger_data = serde_json::json!({"id":"continue-item"});
    runs_proxy
        .call::<runs::StartRunResponse>("StartRun", (start,))
        .await
        .unwrap();

    let events = poll_until_done(&runs_proxy, &owner.storage, "continue-failure-run").await;
    let finished_event = events.events.last().expect("terminal event is persisted");
    let finished: runs::RunFinished = serde_json::from_value(finished_event.data.clone()).unwrap();
    assert_eq!(finished.status, "failed");
    assert!(!finished.successful);
    let error = finished
        .error
        .as_deref()
        .expect("continued failure is explained");
    assert_eq!(error, "node(s) failed after retries: agent");
    assert!(
        finished
            .output
            .as_ref()
            .is_some_and(serde_json::Value::is_object),
        "continue policy must retain the engine outcome"
    );

    let row: Option<catalog::FlowRun> = catalog_proxy
        .call(
            "GetRun",
            (RunIdRequest {
                storage: owner.storage,
                id: "continue-failure-run".into(),
            },),
        )
        .await
        .unwrap();
    let row = row.expect("settled run row persists");
    assert_eq!(row.status, "failed");
    assert_eq!(row.error.as_deref(), Some(error));
}

#[tokio::test]
#[ignore = "run through scripts/verify-module-artifact.sh"]
async fn pending_approval_takes_precedence_over_a_continued_step_error() {
    let (_module_host, connection, catalog_proxy, runs_proxy) =
        super::run_lifecycle_tests::module_proxies().await;
    let owner = owner(
        &connection,
        "continue-then-approval",
        None,
        None,
        None,
        None,
    )
    .await;
    let graph = serde_json::json!({
        "inputs": [{"name":"declared", "type":"string", "required":true}],
        "nodes": [
            {"id":"trigger", "name":"Trigger", "kind":"trigger", "config":{}},
            {"id":"agent", "name":"Agent", "kind":"agent", "config":{"agent_ref":"test-agent","prompt":"work","on_error":"continue"}},
            {"id":"approval", "name":"Approval", "kind":"approval", "config":{"request_id":"review-after-error"}}
        ],
        "edges": [
            {"from_node":"trigger", "from_port":"main", "to_node":"agent", "to_port":"main"},
            {"from_node":"agent", "from_port":"main", "to_node":"approval", "to_port":"main"}
        ]
    });
    let flow_id = create_flow_with_graph(
        &catalog_proxy,
        &owner.storage,
        "Continue then approval",
        graph.clone(),
    )
    .await;
    let _agent = serve_failing_agent(&connection, "continue-then-approval").await;
    let mut start = request(&owner, flow_id, "continue-approval-run", graph, 20_000);
    start.trigger_data = serde_json::json!({"id":"approval-item"});
    runs_proxy
        .call::<runs::StartRunResponse>("StartRun", (start,))
        .await
        .unwrap();

    let events = poll_until_kind(
        &runs_proxy,
        &owner.storage,
        "continue-approval-run",
        "parked",
    )
    .await;
    let parked_event = events
        .iter()
        .find(|event| event.kind == "parked")
        .expect("the downstream approval parks the continued run");
    let parked: runs::RunParked = serde_json::from_value(parked_event.data.clone()).unwrap();
    assert_eq!(parked.pending_approvals, ["approval"]);

    let row: Option<catalog::FlowRun> = catalog_proxy
        .call(
            "GetRun",
            (RunIdRequest {
                storage: owner.storage,
                id: "continue-approval-run".into(),
            },),
        )
        .await
        .unwrap();
    let row = row.expect("parked run row persists");
    assert_eq!(row.status, "pending_approval");
    assert_eq!(
        row.error, None,
        "pending approval has precedence over step error"
    );
}

#[tokio::test]
#[ignore = "run through scripts/verify-module-artifact.sh"]
async fn failed_actor_persists_graph_observations_and_settles_executed_dedup_nodes() {
    let artifact = std::env::var_os("TINYFLOWS_MODULE_ARTIFACT")
        .expect("set TINYFLOWS_MODULE_ARTIFACT to the built cdylib");
    let broker = tinybus::broker::Broker::new();
    let module_host = ModuleHost::new(broker.clone());
    module_host
        .load_file(std::path::PathBuf::from(artifact))
        .unwrap();
    let (client, transport) = MemoryTransport::pair();
    broker.attach(Arc::new(transport));
    let connection = Connection::connect(Box::new(client)).await.unwrap();
    let catalog_proxy = connection
        .proxy(BUS_NAME, catalog::OBJECT_PATH, catalog::INTERFACE)
        .unwrap();
    let runs_proxy = connection
        .proxy(BUS_NAME, runs::OBJECT_PATH, runs::INTERFACE)
        .unwrap();
    let owner = owner(&connection, "failed-run", None, None, None, None).await;
    let graph = dedup_graph("failure-dedup");
    let flow_id =
        create_flow_with_graph(&catalog_proxy, &owner.storage, "Failed flow", graph.clone()).await;
    let _agent = serve_failing_agent(&connection, "failed-run").await;
    let mut request = request(&owner, flow_id.clone(), "failed-run-id", graph, 20_000);
    request.trigger_data = json!({"id":"retry-item"});
    let started: runs::StartRunResponse = runs_proxy.call("StartRun", (request,)).await.unwrap();
    assert!(!started.already_running);

    let events = poll_until_done(&runs_proxy, &owner.storage, "failed-run-id").await;
    assert!(
        events
            .events
            .iter()
            .any(|event| event.kind == "graph_observation" && event.graph_run_id.is_some())
    );
    assert_eq!(events.events.last().unwrap().data["status"], "failed");
    assert!(
        events.events.last().unwrap().data.get("output").is_none(),
        "a failed engine call has no fabricated outcome"
    );
    let finished: runs::RunFinished =
        serde_json::from_value(events.events.last().unwrap().data.clone()).unwrap();
    assert!(!finished.successful);
    let row: Option<catalog::FlowRun> = catalog_proxy
        .call(
            "GetRun",
            (RunIdRequest {
                storage: owner.storage.clone(),
                id: "failed-run-id".into(),
            },),
        )
        .await
        .unwrap();
    let row = row.expect("failed run row persists");
    assert!(
        row.error
            .unwrap()
            .contains("injected agent callback failure")
    );
    let summary: Option<catalog::Workflow> = catalog_proxy
        .call("GetWorkflow", &(owner.storage.clone(), flow_id.clone()))
        .await
        .unwrap();
    assert_eq!(
        summary.unwrap().last_status.as_deref(),
        Some("failed"),
        "failed actor settlement updates the workflow summary"
    );

    let catalog_docs = FlowCatalogDocuments::new(owner.docs.clone());
    let namespace = format!("flow:{flow_id}");
    assert_eq!(
        catalog_docs
            .kv_get(&namespace, "dedup:failure-dedup:tentative:failed-run-id")
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        catalog_docs
            .kv_get(&namespace, "dedup:failure-dedup:committed")
            .await
            .unwrap(),
        None
    );
}
