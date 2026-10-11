use super::*;
use serde_json::json;
use tinyflows_bus::schedule::CALLBACK_INTERFACE;
use tokio::sync::Mutex;

#[derive(Default)]
struct State {
    records: Mutex<Vec<Value>>,
    completions: Mutex<Vec<Value>>,
    executions: Mutex<Vec<String>>,
    triggers: Mutex<Vec<String>>,
}
struct Callbacks(Arc<State>);
#[tinybus::interface(name = "ai.tinyhumans.tinyflows.ScheduleHost")]
impl Callbacks {
    async fn due_jobs(&self, _now: String) -> Result<Vec<Value>> {
        Ok(vec![])
    }
    async fn claim_slot(&self, _job: Value, _next_run: String) -> Result<()> {
        Ok(())
    }
    async fn record_run(&self, record: Value) -> Result<()> {
        self.0.records.lock().await.push(record);
        Ok(())
    }
    async fn complete(&self, _job: Value, completion: Value) -> Result<()> {
        self.0.completions.lock().await.push(completion);
        Ok(())
    }
    async fn execute(&self, _job: Value, run_id: String) -> Result<Value> {
        self.0.executions.lock().await.push(run_id);
        Ok(json!({"success":true,"output":"executed","retryable":false}))
    }
    async fn failed(&self, _job: Value, _run_id: String, _attempt: Value) -> Result<()> {
        Ok(())
    }
    async fn deliver(&self, _job: Value, _run_id: String, _attempt: Value) -> Result<String> {
        Ok("not_requested".into())
    }
    async fn schedule_trigger(&self, flow_id: String, _run_id: String) -> Result<Value> {
        self.0.triggers.lock().await.push(flow_id);
        Ok(json!({"success":true,"output":"triggered","retryable":false}))
    }
}

async fn connections() -> (Connection, Connection, Arc<State>) {
    let bus = tinybus::transport::memory::MemoryBus::new();
    tinybus::broker::Broker::new().spawn(bus.clone());
    let server = Connection::connect(bus.connect().await.unwrap())
        .await
        .unwrap();
    super::super::service::setup(server.clone()).await.unwrap();
    let host = Connection::connect(bus.connect().await.unwrap())
        .await
        .unwrap();
    let state = Arc::new(State::default());
    host.serve_at(
        "/test/ScheduleHost".try_into().unwrap(),
        Callbacks(state.clone()),
    )
    .await
    .unwrap();
    host.request_name("test.ScheduleHost").await.unwrap();
    (server, host, state)
}
fn callback() -> CallbackAddress {
    CallbackAddress {
        destination: "test.ScheduleHost".into(),
        object_path: "/test/ScheduleHost".into(),
    }
}
fn job(kind: &str) -> Value {
    json!({"id":"one","expression":"","schedule":{"kind":"every","every_ms":60000},"command":"flow-a","prompt":null,"name":null,"job_type":kind,"session_target":"isolated","model":null,"agent_id":null,"enabled":true,"delivery":{"mode":"none"},"delete_after_run":false,"created_at":"2026-01-01T00:00:00Z","next_run":"2026-01-01T00:00:00Z","last_run":null,"last_status":null,"last_output":null})
}

#[tokio::test]
async fn separate_schedule_object_parses_delays_and_rejects_cross_tenant_delivery() {
    let (_server, host, _) = connections().await;
    let proxy = host
        .proxy(
            schedule::BUS_NAME,
            schedule::OBJECT_PATH,
            schedule::INTERFACE,
        )
        .unwrap();
    let delay: i64 = proxy.call("ParseHumanDelay", &("2h",)).await.unwrap();
    assert_eq!(delay, 7_200_000);
    let error = proxy
        .call::<()>(
            "ValidateDelivery",
            &(
                json!({"mode":"announce","channel":"telegram","to":"alice"}),
                None::<Value>,
                Some(vec!["bob"]),
            ),
        )
        .await;
    assert!(error.is_err());
    assert_ne!(CALLBACK_INTERFACE, schedule::INTERFACE);
}

#[tokio::test]
async fn manual_execution_routes_flow_directly_and_preserves_run_identity() {
    let (_server, host, state) = connections().await;
    let proxy = host
        .proxy(
            schedule::BUS_NAME,
            schedule::OBJECT_PATH,
            schedule::INTERFACE,
        )
        .unwrap();
    for kind in ["shell", "flow"] {
        let result: Value = proxy
            .call(
                "ExecuteJob",
                &("scope", callback(), json!({}), job(kind), "stable-run"),
            )
            .await
            .unwrap();
        assert_eq!(result["success"], true);
    }
    assert_eq!(*state.executions.lock().await, ["stable-run"]);
    assert_eq!(*state.triggers.lock().await, ["flow-a"]);
}

#[tokio::test]
async fn manual_scheduler_lifecycle_uses_module_tick_and_drain() {
    let (_server, host, _) = connections().await;
    let proxy = host
        .proxy(
            schedule::BUS_NAME,
            schedule::OBJECT_PATH,
            schedule::INTERFACE,
        )
        .unwrap();
    let request = StartScheduler {
        scope: "manual-scope".into(),
        callback: callback(),
        config: json!({}),
        manual_ticks: true,
        storage: None,
    };
    let handle: String = proxy.call("StartScheduler", &(request,)).await.unwrap();
    let count: usize = proxy.call("TickScheduler", &(&handle,)).await.unwrap();
    assert_eq!(count, 0);
    proxy
        .call::<()>("DrainScheduler", &(&handle,))
        .await
        .unwrap();
    proxy
        .call::<()>("StopScheduler", &(&handle,))
        .await
        .unwrap();
    assert!(
        proxy
            .call::<usize>("TickScheduler", &(&handle,))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn module_owned_job_storage_preserves_policy_updates_and_run_history() {
    let (_server, host, _) = connections().await;
    let proxy = host
        .proxy(
            schedule::BUS_NAME,
            schedule::OBJECT_PATH,
            schedule::INTERFACE,
        )
        .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let storage = json!({"backend":{"kind":"sqlite","path":directory.path().join("jobs.db")},"max_run_history":2,"max_tasks":5});
    let call = |operation| json!({"storage":storage,"operation":operation});
    let created: Value = proxy.call("ScheduleStore", &(call(json!({"op":"add_shell","name":"system_seed","schedule":{"kind":"every","every_ms":60000},"command":"echo seeded"})),)).await.unwrap();
    let id = created["id"].as_str().unwrap();
    let updated: Value = proxy.call("ScheduleStore", &(call(json!({"op":"update","job_id":id,"patch":{"policy":{"retries":4,"single_flight":true}}})),)).await.unwrap();
    assert_eq!(updated["policy"]["retries"], 4);
    let _: Value = proxy.call("ScheduleStore", &(call(json!({"op":"record_run","job_id":id,"started_at":"2026-01-01T00:00:00Z","finished_at":"2026-01-01T00:00:01Z","status":"ok","output":"ran","duration_ms":1000,"delivery_status":"not_requested"})),)).await.unwrap();
    let history: Value = proxy
        .call(
            "ScheduleStore",
            &(call(json!({"op":"list_runs","job_id":id,"limit":10})),),
        )
        .await
        .unwrap();
    assert_eq!(history[0]["output"], "ran");
    let list: Value = proxy
        .call("ScheduleStore", &(call(json!({"op":"list"})),))
        .await
        .unwrap();
    assert_eq!(list[0]["name"], "system_seed");
    assert_eq!(list[0]["policy"]["single_flight"], true);
}

#[path = "../tests/support/document_callbacks.rs"]
mod document_callbacks;

#[tokio::test]
async fn module_document_store_retries_policy_migration_conflicts_without_losing_seeded_jobs() {
    use tinystoragedrivers_core::{MemoryStorage, Precondition, Scope, StorageBackend};
    let (_server, host, _) = connections().await;
    let scoped = MemoryStorage::new().for_scope(&Scope::local()).unwrap();
    let docs = scoped.documents().clone();
    host.serve_at(
        "/test/Documents".try_into().unwrap(),
        document_callbacks::Documents {
            docs: docs.clone(),
            conflict_once: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            atomic_calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        },
    )
    .await
    .unwrap();
    let proxy = host
        .proxy(
            schedule::BUS_NAME,
            schedule::OBJECT_PATH,
            schedule::INTERFACE,
        )
        .unwrap();
    let storage = json!({"backend":{"kind":"documents","callback":{"destination":"test.ScheduleHost","object_path":"/test/Documents"}}});
    let call = |operation| json!({"storage":storage,"operation":operation});
    let created: Value = proxy.call("ScheduleStore", &(call(json!({"op":"add_shell","name":"system_seed","schedule":{"kind":"every","every_ms":60000},"command":"echo seeded"})),)).await.unwrap();
    let id = created["id"].as_str().unwrap();
    docs.put(
        "cron_job_policies",
        id,
        json!({"retries":3,"single_flight":true}),
        Precondition::None,
    )
    .await
    .unwrap();
    // Old job documents have no integrated policy. Remove it to exercise migration.
    let stored = docs.get("cron_jobs", id).await.unwrap().unwrap();
    let mut body = stored.doc.clone();
    body.as_object_mut().unwrap().remove("policy");
    docs.put("cron_jobs", id, body, stored.unchanged())
        .await
        .unwrap();
    // A new callback address injects one atomic CAS conflict.
    let conflict_once = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let atomic_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    host.serve_at(
        "/test/Conflicts".try_into().unwrap(),
        document_callbacks::Documents {
            docs: docs.clone(),
            conflict_once: conflict_once.clone(),
            atomic_calls: atomic_calls.clone(),
        },
    )
    .await
    .unwrap();
    let with_conflict = json!({"storage":{"backend":{"kind":"documents","callback":{"destination":"test.ScheduleHost","object_path":"/test/Conflicts"}}},"operation":{"op":"list"}});
    let list: Value = proxy
        .call("ScheduleStore", &(with_conflict,))
        .await
        .unwrap();
    assert_eq!(list[0]["name"], "system_seed");
    assert!(!conflict_once.load(std::sync::atomic::Ordering::SeqCst));
    assert!(atomic_calls.load(std::sync::atomic::Ordering::SeqCst) >= 2);
    assert_eq!(list[0]["policy"]["retries"], 3);
    assert_eq!(list[0]["policy"]["single_flight"], true);
    assert!(docs.get("cron_job_policies", id).await.unwrap().is_none());
}
