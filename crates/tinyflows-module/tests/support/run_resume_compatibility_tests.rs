use super::*;
use serde_json::{Value, json};
use tinyflows::graph::checkpoint::Checkpointer as _;
use tinyflows_drivers::{DriverCheckpointer, catalog::FlowCatalogDocuments};

fn parked_graph() -> Value {
    json!({
        "inputs": [{"name":"declared","type":"string","required":true}],
        "nodes": [
            {"id":"trigger","name":"Trigger","kind":"trigger","config":{}},
            {"id":"dedup","name":"Dedup","kind":"dedup","config":{"key":"=item.id"}},
            {"id":"approval","name":"Approval","kind":"approval","config":{"request_id":"review-1"}}
        ],
        "edges": [
            {"from_node":"trigger","from_port":"main","to_node":"dedup","to_port":"main"},
            {"from_node":"dedup","from_port":"main","to_node":"approval","to_port":"main"}
        ]
    })
}

fn incompatible_graph() -> Value {
    json!({
        "name":"nested-conditional-fan-in",
        "inputs": [{"name":"declared","type":"string","required":true}],
        "nodes": [
            {"id":"start","name":"Trigger","kind":"trigger","config":{}},
            {"id":"outer","name":"Outer","kind":"condition","config":{"field":"outer"}},
            {"id":"inner","name":"Inner","kind":"condition","config":{"field":"inner"}},
            {"id":"outer_else","name":"Outer else","kind":"transform","config":{"set":{"outer_else":true}}},
            {"id":"inner_else","name":"Inner else","kind":"transform","config":{"set":{"inner_else":true}}},
            {"id":"a","name":"A","kind":"transform","config":{"set":{"a":true}}},
            {"id":"c","name":"C","kind":"transform","config":{"set":{"c":true}}},
            {"id":"merge","name":"Merge","kind":"merge","config":{}}
        ],
        "edges": [
            {"from_node":"start","from_port":"main","to_node":"outer","to_port":"main"},
            {"from_node":"start","from_port":"main","to_node":"c","to_port":"main"},
            {"from_node":"outer","from_port":"true","to_node":"inner","to_port":"main"},
            {"from_node":"outer","from_port":"false","to_node":"outer_else","to_port":"main"},
            {"from_node":"inner","from_port":"true","to_node":"a","to_port":"main"},
            {"from_node":"inner","from_port":"false","to_node":"inner_else","to_port":"main"},
            {"from_node":"a","from_port":"main","to_node":"merge","to_port":"main"},
            {"from_node":"c","from_port":"main","to_node":"merge","to_port":"main"}
        ]
    })
}

async fn update_flow_graph(
    catalog_proxy: &tinybus::Proxy,
    storage: &catalog::CatalogStorage,
    flow_id: &str,
    graph: Value,
) {
    let updated: catalog::PutWorkflowResponse = catalog_proxy
        .call(
            "PutWorkflow",
            &(catalog::PutWorkflowRequest::Update {
                storage: storage.clone(),
                id: flow_id.to_owned(),
                name: "Resume compatibility fixture".into(),
                graph,
                require_approval: false,
                expected_version: None,
                force_disarm_automatic: false,
            },),
        )
        .await
        .unwrap();
    assert!(matches!(updated, catalog::PutWorkflowResponse::Ok(_)));
}

async fn repin_parked_run(
    catalog_proxy: &tinybus::Proxy,
    owner: &run_artifact_support::Owner,
    flow_id: &str,
    run_id: &str,
) {
    let flow: Option<catalog::Workflow> = catalog_proxy
        .call("GetWorkflow", &(owner.storage.clone(), flow_id.to_owned()))
        .await
        .unwrap();
    let flow = flow.expect("updated workflow remains in Catalog");
    let graph: tinyflows::model::WorkflowGraph =
        tinyflows::migrate::deserialize_graph(flow.graph.clone()).unwrap();
    let documents = FlowCatalogDocuments::new(owner.docs.clone());
    let row = documents
        .get_flow_run(run_id)
        .await
        .unwrap()
        .expect("the actor parked a real run");
    let graph_hash =
        tinyflows_catalog::graph_hash::compute_graph_hash(&graph, flow.require_approval)
            .expect("compatibility fixture graph hashes");
    assert!(
        documents
            .finish_flow_run(
                run_id,
                "pending_approval",
                &chrono::Utc::now().to_rfc3339(),
                &row.steps,
                &row.pending_approvals,
                None,
                Some(&graph_hash),
            )
            .await
            .unwrap(),
        "the intentional legacy fixture update must keep the row parked"
    );
}

async fn assert_failed_compatibility_refusal(
    owner: &run_artifact_support::Owner,
    catalog_proxy: &tinybus::Proxy,
    runs_proxy: &tinybus::Proxy,
    flow_id: &str,
    run_id: &str,
    reason: &str,
) {
    let terminal = poll_until_done(runs_proxy, &owner.storage, run_id).await;
    assert!(terminal.done);
    let finished: Vec<_> = terminal
        .events
        .iter()
        .filter(|event| event.kind == "finished")
        .collect();
    assert_eq!(finished.len(), 1, "{terminal:?}");
    let event = finished[0];
    assert_eq!(terminal.next_cursor, event.seq);
    let payload: runs::RunFinished = serde_json::from_value(event.data.clone()).unwrap();
    assert_eq!(payload.status, "failed");
    assert!(!payload.successful);
    assert_eq!(payload.error.as_deref(), Some(reason));
    assert!(
        payload.output.is_none(),
        "preflight refusal has no engine output"
    );
    assert!(!payload.timed_out);

    let row: Option<catalog::FlowRun> = catalog_proxy
        .call(
            "GetRun",
            (RunIdRequest {
                storage: owner.storage.clone(),
                id: run_id.to_owned(),
            },),
        )
        .await
        .unwrap();
    let row = row.expect("failed run row remains queryable");
    assert_eq!(row.status, "failed");
    assert!(row.pending_approvals.is_empty());
    assert_eq!(row.error.as_deref(), Some(reason));

    let flow: Option<catalog::Workflow> = catalog_proxy
        .call("GetWorkflow", &(owner.storage.clone(), flow_id.to_owned()))
        .await
        .unwrap();
    assert_eq!(flow.unwrap().last_status.as_deref(), Some("failed"));

    let namespace = format!("flow:{flow_id}");
    let documents = FlowCatalogDocuments::new(owner.docs.clone());
    assert_eq!(
        documents
            .kv_get(&namespace, &format!("dedup:dedup:tentative:{run_id}"))
            .await
            .unwrap(),
        None,
        "a refused run releases its tentative dedup key"
    );
    let checkpointer = DriverCheckpointer::<Value>::new(owner.docs.clone());
    assert!(
        checkpointer.get(run_id, None).await.unwrap().is_none(),
        "a failed compatibility refusal drops the parked checkpoint"
    );
}

async fn start_parked_run(
    connection: &Connection,
    catalog_proxy: &tinybus::Proxy,
    runs_proxy: &tinybus::Proxy,
    owner: &run_artifact_support::Owner,
    name: &str,
    run_id: &str,
) -> String {
    let graph = parked_graph();
    let flow_id = create_flow_with_graph(catalog_proxy, &owner.storage, name, graph.clone()).await;
    let caps_name = name.to_ascii_lowercase().replace(' ', "-");
    let _caps = serve_agent(connection, &caps_name).await;
    let mut start = request(owner, flow_id.clone(), run_id, graph, 20_000);
    start.trigger_data = json!({"id":"compatibility-item"});
    runs_proxy
        .call::<runs::StartRunResponse>("StartRun", (start,))
        .await
        .unwrap();
    poll_until_kind(runs_proxy, &owner.storage, run_id, "parked").await;
    flow_id
}

#[tokio::test]
#[ignore = "run through scripts/verify-module-artifact.sh"]
async fn resume_fails_and_settles_when_current_graph_is_engine_incompatible() {
    let (_module_host, connection, catalog_proxy, runs_proxy) =
        super::run_lifecycle_tests::module_proxies().await;
    let owner = owner(
        &connection,
        "resume-incompatible-graph",
        None,
        None,
        None,
        None,
    )
    .await;
    let run_id = "incompatible-current-graph";
    let flow_id = start_parked_run(
        &connection,
        &catalog_proxy,
        &runs_proxy,
        &owner,
        "resume-incompatible-graph",
        run_id,
    )
    .await;
    let graph = incompatible_graph();
    update_flow_graph(&catalog_proxy, &owner.storage, &flow_id, graph.clone()).await;
    repin_parked_run(&catalog_proxy, &owner, &flow_id, run_id).await;

    let error = runs_proxy
        .call::<runs::ResumeRunResponse>(
            "ResumeRun",
            (runs::ResumeRun {
                run_id: run_id.into(),
                flow_id: flow_id.clone(),
                storage: owner.storage.clone(),
                caps_host: runs::CapsHost {
                    callback: owner.caps.clone(),
                },
                approvals: vec!["approval".into()],
                rejections: Vec::new(),
                timeout_ms: 20_000,
            },),
        )
        .await
        .unwrap_err()
        .wire_message();
    assert!(
        error.contains(tinyflows::compat::UNSUPPORTED_NESTED_CONDITIONAL_FAN_IN),
        "{error}"
    );
    assert_failed_compatibility_refusal(
        &owner,
        &catalog_proxy,
        &runs_proxy,
        &flow_id,
        run_id,
        &error,
    )
    .await;
}

#[tokio::test]
#[ignore = "run through scripts/verify-module-artifact.sh"]
async fn resume_fails_and_settles_when_current_graph_references_incompatible_child() {
    let (_module_host, connection, catalog_proxy, runs_proxy) =
        super::run_lifecycle_tests::module_proxies().await;
    let owner = owner(
        &connection,
        "resume-incompatible-child",
        None,
        None,
        None,
        None,
    )
    .await;
    let run_id = "incompatible-saved-child";
    let flow_id = start_parked_run(
        &connection,
        &catalog_proxy,
        &runs_proxy,
        &owner,
        "resume-incompatible-child",
        run_id,
    )
    .await;
    let child_graph = incompatible_graph();
    let child_id = create_flow_with_graph(
        &catalog_proxy,
        &owner.storage,
        "Legacy incompatible child",
        child_graph,
    )
    .await;
    let graph = json!({
        "inputs": [{"name":"declared","type":"string","required":true}],
        "nodes": [
            {"id":"trigger","name":"Trigger","kind":"trigger","config":{}},
            {"id":"saved-child","name":"Saved child","kind":"sub_workflow","config":{"workflow_id":child_id}}
        ],
        "edges": [
            {"from_node":"trigger","from_port":"main","to_node":"saved-child","to_port":"main"}
        ]
    });
    update_flow_graph(&catalog_proxy, &owner.storage, &flow_id, graph.clone()).await;
    repin_parked_run(&catalog_proxy, &owner, &flow_id, run_id).await;

    let error = runs_proxy
        .call::<runs::ResumeRunResponse>(
            "ResumeRun",
            (runs::ResumeRun {
                run_id: run_id.into(),
                flow_id: flow_id.clone(),
                storage: owner.storage.clone(),
                caps_host: runs::CapsHost {
                    callback: owner.caps.clone(),
                },
                approvals: vec!["approval".into()],
                rejections: Vec::new(),
                timeout_ms: 20_000,
            },),
        )
        .await
        .unwrap_err()
        .wire_message();
    assert!(
        error.starts_with("incompatible_referenced_workflow: "),
        "the Runs error should retain the prior host validation code prefix: {error}"
    );
    assert!(error.contains(child_id.as_str()), "{error}");
    assert!(
        error.contains(tinyflows::compat::UNSUPPORTED_NESTED_CONDITIONAL_FAN_IN),
        "{error}"
    );
    assert_failed_compatibility_refusal(
        &owner,
        &catalog_proxy,
        &runs_proxy,
        &flow_id,
        run_id,
        &error,
    )
    .await;
}
