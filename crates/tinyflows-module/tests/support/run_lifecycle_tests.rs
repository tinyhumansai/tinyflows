use super::*;
use tinyflows_drivers::catalog::FlowCatalogDocuments;
use tokio::sync::Notify;

pub(super) async fn module_proxies() -> (ModuleHost, Connection, tinybus::Proxy, tinybus::Proxy) {
    let artifact = std::env::var_os("TINYFLOWS_MODULE_ARTIFACT")
        .expect("set TINYFLOWS_MODULE_ARTIFACT to the built cdylib");
    let broker = Broker::new();
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
    (module_host, connection, catalog_proxy, runs_proxy)
}

#[tokio::test]
#[ignore = "run through scripts/verify-module-artifact.sh"]
async fn compiled_module_expires_parked_runs_and_sweeps_only_local_orphans() {
    let (_module_host, connection, catalog_proxy, runs_proxy) = module_proxies().await;
    let owner = owner(&connection, "lifecycle", None, None, None, None).await;
    let flow_id = create_flow(&catalog_proxy, &owner.storage, "Lifecycle").await;
    let documents = FlowCatalogDocuments::new(owner.docs.clone());

    documents
        .insert_flow_run("expired", &flow_id, "expired", "2020-01-01T00:00:00Z")
        .await
        .unwrap();
    documents
        .finish_flow_run(
            "expired",
            "pending_approval",
            "2020-01-01T00:01:00Z",
            &[],
            &["gate".into()],
            None,
            Some("graph-pin"),
        )
        .await
        .unwrap();
    let expiry: runs::ExpireParkedResponse = runs_proxy
        .call(
            "ExpireParked",
            (runs::ExpireParked {
                storage: owner.storage.clone(),
            },),
        )
        .await
        .unwrap();
    assert_eq!(expiry.expired, [("expired".into(), flow_id.clone())]);
    let expired: Option<catalog::FlowRun> = catalog_proxy
        .call(
            "GetRun",
            (RunIdRequest {
                storage: owner.storage.clone(),
                id: "expired".into(),
            },),
        )
        .await
        .unwrap();
    assert_eq!(expired.unwrap().status, "cancelled");
    let expired_page: runs::PollRun = runs_proxy
        .call(
            "PollRun",
            (RunControlRequest {
                storage: owner.storage.clone(),
                run_id: "expired".into(),
                cursor: 0,
            },),
        )
        .await
        .unwrap();
    assert!(expired_page.done);
    assert_eq!(
        expired_page.events.last().unwrap().data["status"],
        "cancelled"
    );

    documents
        .insert_flow_run("orphan", &flow_id, "orphan", "2020-01-01T00:00:00Z")
        .await
        .unwrap();
    let shared: runs::BootSweepResponse = runs_proxy
        .call(
            "BootSweep",
            (runs::BootSweep {
                storage: owner.storage.clone(),
                shared_backend: true,
            },),
        )
        .await
        .unwrap();
    assert!(shared.interrupted.is_empty());
    let local: runs::BootSweepResponse = runs_proxy
        .call(
            "BootSweep",
            (runs::BootSweep {
                storage: owner.storage.clone(),
                shared_backend: false,
            },),
        )
        .await
        .unwrap();
    assert_eq!(local.interrupted, ["orphan"]);
    let orphan_page: runs::PollRun = runs_proxy
        .call(
            "PollRun",
            (RunControlRequest {
                storage: owner.storage,
                run_id: "orphan".into(),
                cursor: 0,
            },),
        )
        .await
        .unwrap();
    assert!(orphan_page.done);
    assert_eq!(
        orphan_page.events.last().unwrap().data["status"],
        "interrupted"
    );
    let finished: runs::RunFinished =
        serde_json::from_value(orphan_page.events.last().unwrap().data.clone()).unwrap();
    assert!(!finished.successful);
}

#[tokio::test]
#[ignore = "run through scripts/verify-module-artifact.sh"]
async fn expiry_keeps_processing_rows_after_one_row_write_fails() {
    let (_module_host, connection, catalog_proxy, runs_proxy) = module_proxies().await;
    let owner =
        owner_with_lifecycle_gates(&connection, "expiry-batch", Some("b-expired"), None).await;
    let flow_id = create_flow(&catalog_proxy, &owner.storage, "Expiry batch").await;
    let documents = FlowCatalogDocuments::new(owner.docs.clone());
    for run_id in ["a-expired", "b-expired"] {
        documents
            .insert_flow_run(run_id, &flow_id, run_id, "2020-01-01T00:00:00Z")
            .await
            .unwrap();
        documents
            .finish_flow_run(
                run_id,
                "pending_approval",
                "2020-01-01T00:01:00Z",
                &[],
                &["gate".into()],
                None,
                None,
            )
            .await
            .unwrap();
    }

    assert!(
        runs_proxy
            .call::<runs::ExpireParkedResponse>(
                "ExpireParked",
                (runs::ExpireParked {
                    storage: owner.storage.clone(),
                },),
            )
            .await
            .is_err()
    );
    let first: Option<catalog::FlowRun> = catalog_proxy
        .call(
            "GetRun",
            (RunIdRequest {
                storage: owner.storage.clone(),
                id: "a-expired".into(),
            },),
        )
        .await
        .unwrap();
    assert_eq!(first.unwrap().status, "cancelled");
    let first_page: runs::PollRun = runs_proxy
        .call(
            "PollRun",
            (RunControlRequest {
                storage: owner.storage.clone(),
                run_id: "a-expired".into(),
                cursor: 0,
            },),
        )
        .await
        .unwrap();
    assert!(first_page.done);
    let second: Option<catalog::FlowRun> = catalog_proxy
        .call(
            "GetRun",
            (RunIdRequest {
                storage: owner.storage.clone(),
                id: "b-expired".into(),
            },),
        )
        .await
        .unwrap();
    assert_eq!(second.unwrap().status, "pending_approval");

    let retry: runs::ExpireParkedResponse = runs_proxy
        .call(
            "ExpireParked",
            (runs::ExpireParked {
                storage: owner.storage.clone(),
            },),
        )
        .await
        .unwrap();
    assert_eq!(retry.expired, [("b-expired".into(), flow_id)]);
    let second_page: runs::PollRun = runs_proxy
        .call(
            "PollRun",
            (RunControlRequest {
                storage: owner.storage,
                run_id: "b-expired".into(),
                cursor: 0,
            },),
        )
        .await
        .unwrap();
    assert!(second_page.done);
}

#[tokio::test]
#[ignore = "run through scripts/verify-module-artifact.sh"]
async fn expiry_via_an_alias_retires_the_parked_actor_lease() {
    let (_module_host, connection, catalog_proxy, runs_proxy) = module_proxies().await;
    let owner = owner(&connection, "expiry-owner", None, None, None, None).await;
    let graph = parked_graph();
    let flow_id = create_flow_with_graph(
        &catalog_proxy,
        &owner.storage,
        "Alias expiry",
        graph.clone(),
    )
    .await;
    let callback = serve_agent(&connection, "expiry-owner").await;
    let mut start = request(&owner, flow_id.clone(), "alias-expiry", graph, 20_000);
    start.trigger_data = json!({"id":"alias-expiry-item","payload":"review"});
    runs_proxy
        .call::<runs::StartRunResponse>("StartRun", (start,))
        .await
        .unwrap();
    wait_notify(&callback.approval_started, "approval callback").await;
    poll_until_kind(&runs_proxy, &owner.storage, "alias-expiry", "parked").await;
    let documents = FlowCatalogDocuments::new(owner.docs.clone());
    let row = documents
        .get_flow_run("alias-expiry")
        .await
        .unwrap()
        .unwrap();
    assert!(
        documents
            .finish_flow_run(
                "alias-expiry",
                "pending_approval",
                "2020-01-01T00:01:00Z",
                &row.steps,
                &row.pending_approvals,
                None,
                row.graph_hash.as_deref(),
            )
            .await
            .unwrap()
    );
    let alias = alias_owner(&connection, "expiry-owner-alias", owner.docs.clone()).await;
    let expiry: runs::ExpireParkedResponse = runs_proxy
        .call(
            "ExpireParked",
            (runs::ExpireParked {
                storage: alias.storage.clone(),
            },),
        )
        .await
        .unwrap();
    assert_eq!(expiry.expired, [("alias-expiry".into(), flow_id)]);
    let terminal: runs::PollRun = runs_proxy
        .call(
            "PollRun",
            (RunControlRequest {
                storage: alias.storage,
                run_id: "alias-expiry".into(),
                cursor: 0,
            },),
        )
        .await
        .unwrap();
    assert!(terminal.done);
    assert_eq!(terminal.events.last().unwrap().data["status"], "cancelled");
}

#[tokio::test]
#[ignore = "run through scripts/verify-module-artifact.sh"]
async fn stale_refusal_does_not_mark_the_run_done_until_dedup_cleanup_finishes() {
    let cleanup_started = Arc::new(Notify::new());
    let release_cleanup = Arc::new(Notify::new());
    let (_module_host, connection, catalog_proxy, runs_proxy) = module_proxies().await;
    let owner = owner_with_lifecycle_gates(
        &connection,
        "blocked-cleanup",
        None,
        Some((cleanup_started.clone(), release_cleanup.clone())),
    )
    .await;
    let mut graph = parked_graph();
    let flow_id = create_flow_with_graph(
        &catalog_proxy,
        &owner.storage,
        "Blocked cleanup",
        graph.clone(),
    )
    .await;
    let callback = serve_agent(&connection, "blocked-cleanup").await;
    let mut start = request(
        &owner,
        flow_id.clone(),
        "blocked-cleanup",
        graph.clone(),
        20_000,
    );
    start.trigger_data = json!({"id":"blocked-item","payload":"review"});
    runs_proxy
        .call::<runs::StartRunResponse>("StartRun", (start,))
        .await
        .unwrap();
    wait_notify(&callback.approval_started, "approval callback").await;
    poll_until_kind(&runs_proxy, &owner.storage, "blocked-cleanup", "parked").await;
    graph["nodes"][2]["config"]["request_id"] = json!("changed");
    catalog_proxy
        .call::<catalog::PutWorkflowResponse>(
            "PutWorkflow",
            &(catalog::PutWorkflowRequest::Update {
                storage: owner.storage.clone(),
                id: flow_id.clone(),
                name: "Blocked cleanup changed".into(),
                graph,
                require_approval: false,
                expected_version: None,
                force_disarm_automatic: false,
            },),
        )
        .await
        .unwrap();
    let proxy = runs_proxy.clone();
    let resume_storage = owner.storage.clone();
    let resume_caps = owner.caps.clone();
    let resume = tokio::spawn(async move {
        proxy
            .call::<runs::ResumeRunResponse>(
                "ResumeRun",
                (runs::ResumeRun {
                    run_id: "blocked-cleanup".into(),
                    flow_id,
                    storage: resume_storage,
                    caps_host: runs::CapsHost {
                        callback: resume_caps,
                    },
                    approvals: vec!["approval".into()],
                    rejections: Vec::new(),
                    timeout_ms: 20_000,
                },),
            )
            .await
    });
    wait_notify(&cleanup_started, "dedup cleanup").await;
    let during_cleanup: runs::PollRun = runs_proxy
        .call(
            "PollRun",
            (RunControlRequest {
                storage: owner.storage.clone(),
                run_id: "blocked-cleanup".into(),
                cursor: 0,
            },),
        )
        .await
        .unwrap();
    assert!(!during_cleanup.done);
    assert!(
        during_cleanup
            .events
            .iter()
            .all(|event| event.kind != "finished")
    );
    release_cleanup.notify_one();
    assert!(resume.await.unwrap().is_err());
    let terminal = poll_until_done(&runs_proxy, &owner.storage, "blocked-cleanup").await;
    assert_eq!(terminal.events.last().unwrap().data["status"], "cancelled");
}
