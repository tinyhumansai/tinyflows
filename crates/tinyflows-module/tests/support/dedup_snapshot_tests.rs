use super::*;

#[tokio::test]
#[ignore = "run through scripts/verify-module-artifact.sh"]
async fn dedup_cancellation_settles_against_the_executed_graph_snapshot() {
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
    let owner = owner(&connection, "dedup-snapshot", None, None, None, None).await;
    let original = dedup_graph("original-dedup");
    let flow_id = create_flow_with_graph(
        &catalog_proxy,
        &owner.storage,
        "Dedup flow",
        original.clone(),
    )
    .await;
    let agent = serve_agent(&connection, "dedup-snapshot").await;
    let mut start = request(&owner, flow_id.clone(), "dedup-run", original, 20_000);
    start.trigger_data = json!({"id":"item-1","payload":"source"});
    let admitted: runs::StartRunResponse = runs_proxy.call("StartRun", (start,)).await.unwrap();
    assert!(!admitted.already_running);
    wait_notify(&agent.started, "dedup test agent start").await;
    let steps = poll_until_kind(&runs_proxy, &owner.storage, "dedup-run", "step").await;
    assert_eq!(
        steps
            .iter()
            .find(|event| event.kind == "step")
            .unwrap()
            .data["node_id"],
        "original-dedup",
        "step events: {steps:?}"
    );

    let catalog_docs = tinyflows_drivers::catalog::FlowCatalogDocuments::new(owner.docs.clone());
    let state_namespace = format!("flow:{flow_id}");
    assert_eq!(
        catalog_docs
            .kv_get(&state_namespace, "dedup:original-dedup:tentative:dedup-run")
            .await
            .unwrap(),
        Some(json!(["item-1"]))
    );

    let saved: Option<catalog::Workflow> = catalog_proxy
        .call("GetWorkflow", (owner.storage.clone(), flow_id.clone()))
        .await
        .unwrap();
    let saved = saved.expect("saved workflow exists");
    let update: catalog::PutWorkflowResponse = catalog_proxy
        .call(
            "PutWorkflow",
            &(catalog::PutWorkflowRequest::Update {
                storage: owner.storage.clone(),
                id: flow_id.clone(),
                name: saved.name,
                graph: dedup_graph("edited-dedup"),
                require_approval: saved.require_approval,
                expected_version: Some(saved.updated_at),
                force_disarm_automatic: false,
            }),
        )
        .await
        .unwrap();
    assert!(matches!(update, catalog::PutWorkflowResponse::Ok(_)));

    let cancelled: runs::CancelRunResponse = runs_proxy
        .call(
            "CancelRun",
            (RunControlRequest {
                storage: owner.storage.clone(),
                run_id: "dedup-run".into(),
                cursor: 0,
            },),
        )
        .await
        .unwrap();
    assert!(cancelled.signalled);
    agent.release.notify_one();
    let terminal = poll_until_done(&runs_proxy, &owner.storage, "dedup-run").await;
    assert_eq!(terminal.events.last().unwrap().data["status"], "cancelled");
    assert_eq!(
        catalog_docs
            .kv_get(&state_namespace, "dedup:original-dedup:tentative:dedup-run")
            .await
            .unwrap(),
        None,
        "settlement must release the node that ran from the captured graph"
    );
    assert_eq!(
        catalog_docs
            .kv_get(&state_namespace, "dedup:edited-dedup:tentative:dedup-run")
            .await
            .unwrap(),
        None
    );
}
