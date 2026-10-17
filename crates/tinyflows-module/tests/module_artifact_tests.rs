#[path = "support/authoring_contract_calls.rs"]
mod authoring_contract_calls;
#[path = "support/authoring_surface_artifact.rs"]
mod authoring_surface_artifact;
#[path = "support/schedule_artifact_callbacks.rs"]
mod callbacks;
#[path = "support/copilot_artifact.rs"]
mod copilot_artifact;
#[path = "support/document_callbacks.rs"]
mod document_callbacks;
#[path = "support/manual_worker_artifact_tests.rs"]
mod manual_worker_artifact_tests;
#[path = "support/portability_artifact.rs"]
mod portability_artifact;
#[path = "support/run_artifact_tests.rs"]
mod run_artifact_tests;
#[path = "support/schedule_v4_artifact.rs"]
mod schedule_v4_artifact;
#[path = "support/schedule_v4_delivery_artifact.rs"]
mod schedule_v4_delivery_artifact;
#[path = "support/schedule_v4_manual_artifact.rs"]
mod schedule_v4_manual_artifact;
#[path = "support/schedule_v4_recovery_artifact.rs"]
mod schedule_v4_recovery_artifact;
#[path = "support/storage_migration_artifact_tests.rs"]
mod storage_migration_artifact_tests;
#[path = "support/wiring_warning_artifact.rs"]
mod wiring_warning_artifact;
use serde_json::json;
use std::sync::Arc;
use tinybus::Connection;
use tinybus::broker::Broker;
use tinybus::module::ModuleHost;
use tinybus::transport::memory::MemoryTransport;
use tinyflows_bus::{BUS_NAME, INTERFACE, METHODS};

struct HostCallback;

#[tinybus::interface(name = "ai.tinyhumans.tinyflows.AuthoringHost")]
impl HostCallback {
    async fn resolve_workflow(&self, _: String) -> tinybus::Result<Option<serde_json::Value>> {
        Ok(None)
    }
    async fn check_gate(
        &self,
        stage: String,
        _: serde_json::Value,
    ) -> tinybus::Result<Vec<String>> {
        Ok(if stage == "connections" {
            vec!["artifact callback refused account".into()]
        } else {
            vec![]
        })
    }
    async fn lookup_tool_contract(
        &self,
        slug: String,
    ) -> tinybus::Result<Option<tinyflows_bus::ToolContract>> {
        Ok(
            (slug == "provider:issues").then(|| tinyflows_bus::ToolContract {
                output_fields: vec!["issues".into()],
                output_schema: Some(json!({"type":"object"})),
                primary_array_path: Some("data.issues".into()),
                envelope_prefix: Some("data".into()),
                metadata_fields: vec!["request_id".into()],
            }),
        )
    }
}

#[tokio::test]
#[ignore = "run through scripts/verify-module-artifact.sh"]
async fn compiled_module_serves_validation_without_linking_engine_into_client() {
    assert_eq!(tinyflows_bus::CONTRACT_VERSION, 3);
    assert_eq!(tinyflows_bus::catalog::CONTRACT_VERSION, 5);
    assert_eq!(tinyflows_bus::runs::CONTRACT_VERSION, 5);
    assert_eq!(tinyflows_bus::portability::CONTRACT_VERSION, 2);
    assert_eq!(tinyflows_bus::schedule::CONTRACT_VERSION, 6);
    assert_eq!(tinyflows_bus::runs::CAPS_CONTRACT_VERSION, 1);
    assert_eq!(tinyflows_bus::runs::CAPS_METHODS.len(), 19);
    let artifact = std::env::var_os("TINYFLOWS_MODULE_ARTIFACT")
        .expect("set TINYFLOWS_MODULE_ARTIFACT to the built cdylib");
    let broker = Broker::new();
    let host = ModuleHost::new(broker.clone());
    let info = host.load_file(std::path::PathBuf::from(artifact)).unwrap();
    assert!(
        info.manifest
            .requires
            .iter()
            .any(|dependency| dependency.optional
                && dependency.interface.interface.as_str() == tinyflows_bus::HOST_INTERFACE)
    );
    assert!(info.manifest.requires.iter().any(|dependency| {
        dependency.optional
            && dependency.interface.interface.as_str() == tinyflows_bus::catalog::STORAGE_INTERFACE
    }));
    let copilot_interface = info
        .manifest
        .provides
        .iter()
        .find(|provided| provided.version.interface.as_str() == tinyflows_bus::copilot::INTERFACE)
        .expect("copilot interface is provided by the compiled module");
    let actual_copilot_methods: Vec<String> = copilot_interface
        .methods
        .iter()
        .map(ToString::to_string)
        .collect();
    let expected_copilot_methods: Vec<String> = tinyflows_bus::copilot::METHODS
        .iter()
        .map(|method| method.to_string())
        .collect();
    assert_eq!(actual_copilot_methods, expected_copilot_methods);
    let portability_interface = info
        .manifest
        .provides
        .iter()
        .find(|provided| {
            provided.version.interface.as_str() == tinyflows_bus::portability::INTERFACE
        })
        .expect("portability interface is provided by the compiled module");
    let mut actual_portability: Vec<_> = portability_interface
        .methods
        .iter()
        .map(ToString::to_string)
        .collect();
    let mut expected_portability: Vec<_> = tinyflows_bus::portability::METHODS
        .iter()
        .map(|member| member.to_string())
        .collect();
    actual_portability.sort();
    expected_portability.sort();
    assert_eq!(actual_portability, expected_portability);
    assert!(info.manifest.requires.iter().any(|dependency| {
        dependency.optional
            && dependency.interface.interface.as_str()
                == tinyflows_bus::catalog::DRAFT_PROMOTION_HOST_INTERFACE
    }));
    assert!(info.manifest.requires.iter().any(|dependency| {
        dependency.optional
            && dependency.interface.interface.as_str()
                == tinyflows_bus::catalog::APPROVAL_MANIFEST_HOST_INTERFACE
    }));
    assert!(info.manifest.requires.iter().any(|dependency| {
        dependency.optional
            && dependency.interface.interface.as_str() == tinyflows_bus::runs::CAPS_INTERFACE
    }));
    let runs_interface = info
        .manifest
        .provides
        .iter()
        .find(|provided| provided.version.interface.as_str() == tinyflows_bus::runs::INTERFACE)
        .expect("run-control interface is provided");
    let mut actual: Vec<_> = runs_interface
        .methods
        .iter()
        .map(ToString::to_string)
        .collect();
    let mut expected: Vec<_> = tinyflows_bus::runs::METHODS
        .iter()
        .map(|member| member.to_string())
        .collect();
    actual.sort();
    expected.sort();
    assert_eq!(actual, expected);
    let provided = info
        .manifest
        .provides
        .iter()
        .find(|provided| provided.version.interface.as_str() == INTERFACE)
        .unwrap();
    let mut actual: Vec<_> = provided.methods.iter().map(ToString::to_string).collect();
    let mut expected: Vec<_> = METHODS.iter().map(|member| member.to_string()).collect();
    actual.sort();
    expected.sort();
    assert_eq!(actual, expected);

    let catalog_interface = info
        .manifest
        .provides
        .iter()
        .find(|provided| provided.version.interface.as_str() == tinyflows_bus::catalog::INTERFACE)
        .unwrap();
    let mut actual: Vec<_> = catalog_interface
        .methods
        .iter()
        .map(ToString::to_string)
        .collect();
    let mut expected: Vec<_> = tinyflows_bus::catalog::METHODS
        .iter()
        .map(|member| member.to_string())
        .collect();
    actual.sort();
    expected.sort();
    assert_eq!(actual, expected);

    let schedule_interface = info
        .manifest
        .provides
        .iter()
        .find(|provided| provided.version.interface.as_str() == tinyflows_bus::schedule::INTERFACE)
        .unwrap();
    let mut actual: Vec<_> = schedule_interface
        .methods
        .iter()
        .map(ToString::to_string)
        .collect();
    let mut expected: Vec<_> = tinyflows_bus::schedule::METHODS
        .iter()
        .map(|s| s.to_string())
        .collect();
    actual.sort();
    expected.sort();
    assert_eq!(actual, expected);

    let (client, transport) = MemoryTransport::pair();
    broker.attach(Arc::new(transport));
    let connection = Connection::connect(Box::new(client)).await.unwrap();
    let schedule_proxy = connection
        .proxy(
            tinyflows_bus::BUS_NAME,
            tinyflows_bus::schedule::OBJECT_PATH,
            tinyflows_bus::schedule::INTERFACE,
        )
        .unwrap();
    let delay: i64 = schedule_proxy
        .call("ParseHumanDelay", ("2h",))
        .await
        .unwrap();
    assert_eq!(delay, 7_200_000);

    let state = Arc::new(callbacks::State::default());
    connection
        .serve_at(
            "/test/ScheduleCallbacks".try_into().unwrap(),
            callbacks::Callbacks(state.clone()),
        )
        .await
        .unwrap();
    connection
        .request_name("test.ScheduleCallbacks")
        .await
        .unwrap();
    let request = tinyflows_bus::schedule::StartScheduler {
        scope: "artifact-scope".into(),
        callback: tinyflows_bus::schedule::CallbackAddress {
            destination: "test.ScheduleCallbacks".into(),
            object_path: "/test/ScheduleCallbacks".into(),
        },
        config: json!({"now": "2027-01-01T00:00:00Z"}),
        manual_ticks: true,
        storage: None,
    };
    let handle: String = schedule_proxy
        .call("StartScheduler", (request,))
        .await
        .unwrap();
    let count: usize = schedule_proxy
        .call("TickScheduler", (&handle,))
        .await
        .unwrap();
    assert_eq!(count, 1);
    schedule_proxy
        .call::<()>("DrainScheduler", (&handle,))
        .await
        .unwrap();
    schedule_proxy
        .call::<()>("StopScheduler", (&handle,))
        .await
        .unwrap();
    assert_eq!(*state.triggers.lock().unwrap(), ["artifact-flow"]);
    assert_eq!(state.records.lock().unwrap()[0]["status"], "ok");
    for field in ["started_at", "finished_at"] {
        assert_eq!(
            state.records.lock().unwrap()[0][field],
            "2027-01-01T00:00:00Z"
        );
    }
    assert_eq!(state.completions.lock().unwrap()[0]["disable"], true);

    assert_eq!(
        state.records.lock().unwrap()[0]["job_created_at"],
        "2026-01-01T00:00:00Z"
    );

    // A held first occurrence must not hide a later job in a one-job batch.
    // Notify handshakes synchronize the compiled runtime with no sleeps.
    let deferred = Arc::new(callbacks::State::default());
    deferred
        .defer_first
        .store(true, std::sync::atomic::Ordering::SeqCst);
    connection
        .serve_at(
            "/test/DeferredScheduleCallbacks".try_into().unwrap(),
            callbacks::Callbacks(deferred.clone()),
        )
        .await
        .unwrap();
    let request = tinyflows_bus::schedule::StartScheduler {
        scope: "artifact-deferred-scope".into(),
        callback: tinyflows_bus::schedule::CallbackAddress {
            destination: "test.ScheduleCallbacks".into(),
            object_path: "/test/DeferredScheduleCallbacks".into(),
        },
        config: json!({}),
        manual_ticks: true,
        storage: None,
    };
    let handle: String = schedule_proxy
        .call("StartScheduler", (request,))
        .await
        .unwrap();
    assert_eq!(
        schedule_proxy
            .call::<usize>("TickScheduler", (&handle,))
            .await
            .unwrap(),
        1
    );
    deferred.started.notified().await;
    assert_eq!(
        schedule_proxy
            .call::<usize>("TickScheduler", (&handle,))
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        *deferred.exclusions.lock().unwrap(),
        [vec!["artifact-flow-job".to_owned()]]
    );
    deferred.release.notify_one();
    schedule_proxy
        .call::<()>("DrainScheduler", (&handle,))
        .await
        .unwrap();
    schedule_proxy
        .call::<()>("StopScheduler", (&handle,))
        .await
        .unwrap();
    assert_eq!(deferred.records.lock().unwrap().len(), 2);
    assert!(
        deferred
            .triggers
            .lock()
            .unwrap()
            .iter()
            .any(|flow| flow == "later-flow")
    );

    // Persist through the actual cdylib. The host forwards document I/O only;
    // schema, policy and schedule behavior is module-owned.
    use tinystoragedrivers_core::{MemoryStorage, Scope, StorageBackend};
    let scoped = MemoryStorage::new().for_scope(&Scope::local()).unwrap();
    let docs = scoped.documents().clone();
    let conflict_once = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let atomic_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    connection
        .serve_at(
            "/test/Documents".try_into().unwrap(),
            document_callbacks::Documents {
                docs: docs.clone(),
                conflict_once: conflict_once.clone(),
                atomic_calls: atomic_calls.clone(),
            },
        )
        .await
        .unwrap();
    let storage = json!({"backend":{"kind":"documents","callback":{"destination":"test.ScheduleCallbacks","object_path":"/test/Documents"}}});
    // Unclaimed jobs preserve the legacy wire bytes, and a job added through the
    // compiled module reads back by id.
    let created: serde_json::Value = schedule_proxy.call("ScheduleStore", (json!({"storage":storage,"operation":{"op":"add_shell","name":"compiled-seed","schedule":{"kind":"every","every_ms":60000},"command":"echo seed"}}),)).await.unwrap();
    let listed: serde_json::Value = schedule_proxy
        .call(
            "ScheduleStore",
            (json!({"storage":storage,"operation":{"op":"list"}}),),
        )
        .await
        .unwrap();
    assert_eq!(listed[0]["id"], created["id"]);
    assert!(
        created.get("claim_generation").is_none(),
        "unclaimed jobs preserve the legacy wire bytes"
    );
    assert!(listed[0].get("claim_generation").is_none());
    let _: serde_json::Value = schedule_proxy
        .call(
            "ScheduleStore",
            (json!({"storage":storage,"operation":{"op":"remove","job_id":created["id"]}}),),
        )
        .await
        .unwrap();
    // Admission retries a real transaction conflict without creating a second job.
    let flow: serde_json::Value = schedule_proxy.call("ScheduleStore", (json!({"storage":storage,"operation":{"op":"add_flow","flow_id":"stored-flow","schedule":{"kind":"every","every_ms":60000}}}),)).await.unwrap();
    assert!(!conflict_once.load(std::sync::atomic::Ordering::SeqCst));
    assert!(atomic_calls.load(std::sync::atomic::Ordering::SeqCst) >= 2);
    let listed: serde_json::Value = schedule_proxy
        .call(
            "ScheduleStore",
            (json!({"storage":storage,"operation":{"op":"list"}}),),
        )
        .await
        .unwrap();
    assert_eq!(listed.as_array().unwrap().len(), 1);
    let id = flow["id"].as_str().unwrap();
    let stored = docs.get("cron.jobs", id).await.unwrap().unwrap();
    let mut body = stored.doc.clone();
    let due_at = chrono::Utc::now() - chrono::Duration::minutes(1);
    body["next_run_ms"] = json!(due_at.timestamp_millis());
    docs.put("cron.jobs", id, body, stored.unchanged())
        .await
        .unwrap();
    let request = tinyflows_bus::schedule::StartScheduler {
        scope: "artifact-stored".into(),
        callback: tinyflows_bus::schedule::CallbackAddress {
            destination: "test.ScheduleCallbacks".into(),
            object_path: "/test/ScheduleCallbacks".into(),
        },
        config: json!({}),
        manual_ticks: true,
        storage: Some(serde_json::from_value(storage.clone()).unwrap()),
    };
    let handle: String = schedule_proxy
        .call("StartScheduler", (request,))
        .await
        .unwrap();
    let count: usize = schedule_proxy
        .call("TickScheduler", (&handle,))
        .await
        .unwrap();
    assert_eq!(count, 1);
    schedule_proxy
        .call::<()>("DrainScheduler", (&handle,))
        .await
        .unwrap();
    schedule_proxy
        .call::<()>("StopScheduler", (&handle,))
        .await
        .unwrap();
    let history: serde_json::Value = schedule_proxy
        .call(
            "ScheduleStore",
            (json!({"storage":storage,"operation":{"op":"list_runs","job_id":id,"limit":10}}),),
        )
        .await
        .unwrap();
    assert_eq!(history[0]["status"], "ok");
    let stored = docs.get("cron.jobs", id).await.unwrap().unwrap();
    assert!(
        stored.doc["next_run_ms"].as_i64().unwrap() > due_at.timestamp_millis(),
        "compiled scheduler advances the occurrence only at terminal completion"
    );
    assert!(stored.doc.get("occurrence_lease").is_none());
    assert_eq!(
        *state.triggers.lock().unwrap(),
        ["artifact-flow", "stored-flow"]
    );
    schedule_v4_artifact::verify_captured_triggers(&state, id);
    assert_eq!(
        state.records.lock().unwrap().len(),
        1,
        "configured storage bypasses the host cron-store callback"
    );

    authoring_surface_artifact::verify(&connection).await;
    schedule_v4_artifact::verify(&schedule_proxy, docs, storage, state).await;
}
