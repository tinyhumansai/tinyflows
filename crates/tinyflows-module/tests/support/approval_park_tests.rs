use super::*;
use serde_json::Value;
use tinyflows::graph::checkpoint::Checkpointer as _;
use tinyflows_drivers::{DriverCheckpointer, catalog::FlowCatalogDocuments};

#[tokio::test]
#[ignore = "run through scripts/verify-module-artifact.sh"]
async fn compiled_module_resumes_a_parked_approval_run() {
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
    let owner = owner(&connection, "resume", None, None, None, None).await;
    let graph = parked_graph();
    let flow_id =
        create_flow_with_graph(&catalog_proxy, &owner.storage, "Resume flow", graph.clone()).await;
    let callback = serve_agent(&connection, "resume").await;
    let mut start = request(&owner, flow_id.clone(), "resume-run", graph, 20_000);
    start.trigger_data = json!({"id":"resume-item","payload":"review"});
    let started: runs::StartRunResponse = runs_proxy.call("StartRun", (start,)).await.unwrap();
    assert!(!started.already_running);
    wait_notify(&callback.approval_started, "approval callback").await;
    poll_until_kind(&runs_proxy, &owner.storage, "resume-run", "parked").await;

    let resumed: runs::ResumeRunResponse = runs_proxy
        .call(
            "ResumeRun",
            (runs::ResumeRun {
                run_id: "resume-run".into(),
                flow_id: flow_id.clone(),
                storage: owner.storage.clone(),
                caps_host: runs::CapsHost {
                    callback: owner.caps,
                },
                approvals: vec!["approval".into()],
                rejections: vec![],
                timeout_ms: 20_000,
            },),
        )
        .await
        .unwrap();
    assert_eq!(resumed.run_id, "resume-run");
    let terminal = poll_until_done(&runs_proxy, &owner.storage, "resume-run").await;
    assert!(terminal.events.iter().any(|event| event.kind == "resumed"));
    assert_eq!(terminal.events.last().unwrap().data["status"], "completed");
    let row: Option<catalog::FlowRun> = catalog_proxy
        .call(
            "GetRun",
            (RunIdRequest {
                storage: owner.storage,
                id: "resume-run".into(),
            },),
        )
        .await
        .unwrap();
    let row = row.expect("resumed run remains in the owner scope");
    assert_eq!(row.status, terminal.events.last().unwrap().data["status"]);
    assert!(row.pending_approvals.is_empty());
    assert_eq!(
        row.steps.len(),
        3,
        "the resumed gate replaces its parked step"
    );
    assert!(row.steps.iter().all(|step| step.diagnostics.is_empty()));
    let approval = row
        .steps
        .iter()
        .find(|step| step.node_id == "approval")
        .expect("resumed approval is recorded");
    assert_eq!(approval.status.as_deref(), Some("success"));
    assert!(approval.diagnostics.is_empty());
    assert!(row.steps.iter().any(|step| step.node_id == "dedup"));
    assert!(row.steps.iter().any(|step| step.node_id == "trigger"));
    let documents = FlowCatalogDocuments::new(owner.docs.clone());
    let namespace = format!("flow:{flow_id}");
    assert_eq!(
        documents
            .kv_get(&namespace, "dedup:dedup:committed")
            .await
            .unwrap(),
        Some(json!(["resume-item"]))
    );
}

#[tokio::test]
#[ignore = "run through scripts/verify-module-artifact.sh"]
async fn compiled_module_guards_resume_ownership_pending_gate_and_graph_pin() {
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
    let owner = owner(&connection, "resume-guards", None, None, None, None).await;
    let graph = parked_graph();
    let flow_id = create_flow_with_graph(
        &catalog_proxy,
        &owner.storage,
        "Guarded resume",
        graph.clone(),
    )
    .await;
    let callback = serve_agent(&connection, "resume-guards").await;
    let mut start = request(
        &owner,
        flow_id.clone(),
        "guarded-run",
        graph.clone(),
        20_000,
    );
    start.trigger_data = json!({"id":"guarded-item","payload":"review"});
    runs_proxy
        .call::<runs::StartRunResponse>("StartRun", (start,))
        .await
        .unwrap();
    wait_notify(&callback.approval_started, "approval callback").await;
    poll_until_kind(&runs_proxy, &owner.storage, "guarded-run", "parked").await;

    let mut wrong_flow = resume_request(&owner, "wrong-flow", "guarded-run", "gate");
    assert!(
        runs_proxy
            .call::<runs::ResumeRunResponse>("ResumeRun", (wrong_flow.clone(),))
            .await
            .is_err()
    );
    wrong_flow.flow_id = flow_id.clone();
    wrong_flow.approvals = vec!["not-pending".into()];
    assert!(
        runs_proxy
            .call::<runs::ResumeRunResponse>("ResumeRun", (wrong_flow,))
            .await
            .is_err()
    );
    let parked: Option<catalog::FlowRun> = catalog_proxy
        .call(
            "GetRun",
            (RunIdRequest {
                storage: owner.storage.clone(),
                id: "guarded-run".into(),
            },),
        )
        .await
        .unwrap();
    assert_eq!(parked.unwrap().status, "pending_approval");

    let mut changed_graph = graph;
    changed_graph["nodes"][2]["config"]["request_id"] = json!("changed-review");
    let updated: catalog::PutWorkflowResponse = catalog_proxy
        .call(
            "PutWorkflow",
            &(catalog::PutWorkflowRequest::Update {
                storage: owner.storage.clone(),
                id: flow_id.clone(),
                name: "Guarded resume changed".into(),
                graph: changed_graph,
                require_approval: false,
                expected_version: None,
                force_disarm_automatic: false,
            },),
        )
        .await
        .unwrap();
    assert!(matches!(updated, catalog::PutWorkflowResponse::Ok(_)));
    let stale_resume = runs_proxy
        .call::<runs::ResumeRunResponse>(
            "ResumeRun",
            (resume_request(&owner, &flow_id, "guarded-run", "approval"),),
        )
        .await;
    let stale_error = stale_resume.unwrap_err();
    assert!(stale_error.to_string().contains("workflow graph changed"));

    let cancelled: Option<catalog::FlowRun> = catalog_proxy
        .call(
            "GetRun",
            (RunIdRequest {
                storage: owner.storage.clone(),
                id: "guarded-run".into(),
            },),
        )
        .await
        .unwrap();
    assert_eq!(
        cancelled.as_ref().unwrap().status,
        "cancelled",
        "stale refusal result: {stale_error:?}; parked hash: {:?}",
        cancelled.as_ref().unwrap().graph_hash
    );
    let terminal: runs::PollRun = runs_proxy
        .call(
            "PollRun",
            (RunControlRequest {
                storage: owner.storage,
                run_id: "guarded-run".into(),
                cursor: 0,
            },),
        )
        .await
        .unwrap();
    assert!(terminal.done);
    assert_eq!(terminal.events.last().unwrap().data["status"], "cancelled");
    let documents = FlowCatalogDocuments::new(owner.docs.clone());
    let namespace = format!("flow:{flow_id}");
    assert_eq!(
        documents
            .kv_get(&namespace, "dedup:dedup:tentative:guarded-run")
            .await
            .unwrap(),
        None
    );
    let checkpointer = DriverCheckpointer::<Value>::new(owner.docs.clone());
    assert!(
        checkpointer
            .get("guarded-run", None)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
#[ignore = "run through scripts/verify-module-artifact.sh"]
async fn compiled_module_keeps_resume_cancellable_and_rejects_a_second_claim() {
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
    let owner = owner(&connection, "resume-cancel", None, None, None, None).await;
    let graph = json!({
        "inputs":[{"name":"declared","type":"string","required":true}],
        "nodes":[
            {"id":"trigger","name":"Trigger","kind":"trigger","config":{}},
            {"id":"approval","name":"Approval","kind":"approval","config":{"request_id":"review"}},
            {"id":"agent","name":"Agent","kind":"agent","config":{"agent_ref":"test-agent","prompt":"work"}},
            {"id":"agent-tail","name":"Tail agent","kind":"agent","config":{"agent_ref":"test-agent","prompt":"tail"}},
            {"id":"output","name":"Output","kind":"transform","config":{"set":{"declared":"=inputs.declared","payload":"=run.trigger.payload"}}}
        ],
        "edges":[
            {"from_node":"trigger","from_port":"main","to_node":"approval","to_port":"main"},
            {"from_node":"approval","from_port":"main","to_node":"agent","to_port":"main"},
            {"from_node":"agent","from_port":"main","to_node":"agent-tail","to_port":"main"},
            {"from_node":"agent-tail","from_port":"main","to_node":"output","to_port":"main"}
        ]
    });
    let flow_id = create_flow_with_graph(
        &catalog_proxy,
        &owner.storage,
        "Cancellable resume",
        graph.clone(),
    )
    .await;
    let agent = serve_agent(&connection, "resume-cancel").await;
    let start = request(&owner, flow_id.clone(), "cancel-resume-run", graph, 20_000);
    runs_proxy
        .call::<runs::StartRunResponse>("StartRun", (start,))
        .await
        .unwrap();
    wait_notify(&agent.approval_started, "approval callback").await;
    poll_until_kind(&runs_proxy, &owner.storage, "cancel-resume-run", "parked").await;

    let resume = resume_request(&owner, &flow_id, "cancel-resume-run", "approval");
    runs_proxy
        .call::<runs::ResumeRunResponse>("ResumeRun", (resume.clone(),))
        .await
        .unwrap();
    wait_notify(&agent.started, "resumed agent callback").await;
    assert!(
        runs_proxy
            .call::<runs::ResumeRunResponse>("ResumeRun", (resume,))
            .await
            .is_err()
    );
    let cancelled: runs::CancelRunResponse = runs_proxy
        .call(
            "CancelRun",
            (RunControlRequest {
                storage: owner.storage.clone(),
                run_id: "cancel-resume-run".into(),
                cursor: 0,
            },),
        )
        .await
        .unwrap();
    assert!(cancelled.signalled);
    agent.release.notify_one();
    let terminal = poll_until_done(&runs_proxy, &owner.storage, "cancel-resume-run").await;
    assert_eq!(terminal.events.last().unwrap().data["status"], "cancelled");
    let row: Option<catalog::FlowRun> = catalog_proxy
        .call(
            "GetRun",
            (RunIdRequest {
                storage: owner.storage,
                id: "cancel-resume-run".into(),
            },),
        )
        .await
        .unwrap();
    let row = row.unwrap();
    assert_eq!(row.status, "cancelled");
    assert_eq!(agent.calls.load(Ordering::SeqCst), 1);
}

fn resume_request(owner: &Owner, flow_id: &str, run_id: &str, gate: &str) -> runs::ResumeRun {
    runs::ResumeRun {
        run_id: run_id.into(),
        flow_id: flow_id.into(),
        storage: owner.storage.clone(),
        caps_host: runs::CapsHost {
            callback: owner.caps.clone(),
        },
        approvals: vec![gate.into()],
        rejections: Vec::new(),
        timeout_ms: 20_000,
    }
}

#[tokio::test]
#[ignore = "run through scripts/verify-module-artifact.sh"]
async fn pending_approval_is_parked_without_terminal_event_or_dedup_settlement() {
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
    let owner = owner(
        &connection,
        "approval-park",
        None,
        None,
        None,
        Some(Arc::new(std::sync::atomic::AtomicBool::new(true))),
    )
    .await;
    let graph = parked_graph();
    let flow_id =
        create_flow_with_graph(&catalog_proxy, &owner.storage, "Parked flow", graph.clone()).await;
    let callback = serve_agent(&connection, "approval-park").await;
    let mut request = request(&owner, flow_id.clone(), "parked-run", graph, 20_000);
    request.trigger_data = json!({"id":"parked-item","payload":"review"});
    let started: runs::StartRunResponse = runs_proxy.call("StartRun", (request,)).await.unwrap();
    assert!(!started.already_running);
    wait_notify(&callback.approval_started, "approval callback").await;

    let events = poll_until_kind(&runs_proxy, &owner.storage, "parked-run", "parked").await;
    assert!(events.iter().all(|event| event.kind != "finished"));
    let parked_page: runs::PollRun = runs_proxy
        .call(
            "PollRun",
            (RunControlRequest {
                storage: owner.storage.clone(),
                run_id: "parked-run".into(),
                cursor: 0,
            },),
        )
        .await
        .unwrap();
    assert!(!parked_page.done);
    let row: Option<catalog::FlowRun> = catalog_proxy
        .call(
            "GetRun",
            (RunIdRequest {
                storage: owner.storage.clone(),
                id: "parked-run".into(),
            },),
        )
        .await
        .unwrap();
    let row = row.expect("parked run row persists");
    assert_eq!(row.status, "pending_approval");
    assert_eq!(row.pending_approvals, ["approval"]);

    let documents = FlowCatalogDocuments::new(owner.docs.clone());
    let namespace = format!("flow:{flow_id}");
    assert_eq!(
        documents
            .kv_get(&namespace, "dedup:dedup:tentative:parked-run")
            .await
            .unwrap(),
        Some(json!(["parked-item"]))
    );
    assert_eq!(
        documents
            .kv_get(&namespace, "dedup:dedup:committed")
            .await
            .unwrap(),
        None
    );
    let checkpointer = DriverCheckpointer::<Value>::new(owner.docs.clone());
    assert!(
        checkpointer
            .get("parked-run", None)
            .await
            .unwrap()
            .is_some()
    );

    let cancelled: runs::CancelRunResponse = runs_proxy
        .call(
            "CancelRun",
            (RunControlRequest {
                storage: owner.storage.clone(),
                run_id: "parked-run".into(),
                cursor: 0,
            },),
        )
        .await
        .unwrap();
    assert!(cancelled.signalled);
    let terminal = poll_until_done(&runs_proxy, &owner.storage, "parked-run").await;
    assert_eq!(terminal.events.last().unwrap().data["status"], "cancelled");
    assert_eq!(
        documents
            .kv_get(&namespace, "dedup:dedup:tentative:parked-run")
            .await
            .unwrap(),
        None
    );
    assert!(
        checkpointer
            .get("parked-run", None)
            .await
            .unwrap()
            .is_some(),
        "a failed best-effort checkpoint deletion does not wedge terminal polling"
    );
    let repeated_cancel: runs::CancelRunResponse = runs_proxy
        .call(
            "CancelRun",
            (RunControlRequest {
                storage: owner.storage.clone(),
                run_id: "parked-run".into(),
                cursor: 0,
            },),
        )
        .await
        .unwrap();
    assert!(!repeated_cancel.signalled);
    let settled: Option<catalog::FlowRun> = catalog_proxy
        .call(
            "GetRun",
            (RunIdRequest {
                storage: owner.storage,
                id: "parked-run".into(),
            },),
        )
        .await
        .unwrap();
    assert_eq!(settled.unwrap().status, "cancelled");
}
