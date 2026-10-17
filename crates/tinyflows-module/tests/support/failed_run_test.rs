use super::*;
use tinyflows_drivers::catalog::FlowCatalogDocuments;

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

    let catalog_docs = FlowCatalogDocuments::new(owner.docs.clone());
    let namespace = format!("flow:{flow_id}");
    assert_eq!(
        catalog_docs
            .kv_get(&namespace, "dedup:failure-dedup:tentative")
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
