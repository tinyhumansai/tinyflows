use super::*;
use serde_json::json;
use tinyflows_bus::schedule::{CALLBACK_INTERFACE, CallbackAddress, CronJob};
use tokio::sync::Mutex;

#[derive(Default)]
pub(super) struct State {
    executions: Mutex<Vec<String>>,
    triggers: Mutex<Vec<String>>,
    defer_execute: std::sync::atomic::AtomicBool,
    execute_started: tokio::sync::Notify,
    execute_release: tokio::sync::Notify,
    fail_deliver: std::sync::atomic::AtomicBool,
}
struct Callbacks(Arc<State>);
#[tinybus::interface(name = "ai.tinyhumans.tinyflows.ScheduleHost")]
impl Callbacks {
    async fn due_jobs(&self, now: chrono::DateTime<chrono::Utc>) -> Result<Vec<Value>> {
        let _ = now;
        Ok(vec![])
    }
    async fn due_jobs_excluding(
        &self,
        now: chrono::DateTime<chrono::Utc>,
        excluded: Vec<String>,
    ) -> Result<Vec<Value>> {
        let _ = excluded;
        self.due_jobs(now).await
    }
    async fn execute(&self, _job: Value, run_id: String) -> Result<Value> {
        self.0.executions.lock().await.push(run_id);
        if self
            .0
            .defer_execute
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            self.0.execute_started.notify_one();
            self.0.execute_release.notified().await;
        }
        Ok(json!({"success":true,"output":"executed","retryable":false}))
    }
    async fn failed(&self, _job: Value, _run_id: String, _attempt: Value) -> Result<()> {
        Ok(())
    }
    async fn send(&self, _job: Value, _run_id: String, _attempt: Value) -> Result<String> {
        Ok("not_requested".into())
    }
    async fn deliver(&self, _projection: Value) -> Result<()> {
        if self
            .0
            .fail_deliver
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(tinybus::Error::failed("injected delivery projection error"));
        }
        Ok(())
    }
    async fn schedule_trigger(&self, job: Value, _run_id: String) -> Result<Value> {
        self.0
            .triggers
            .lock()
            .await
            .push(job["command"].as_str().unwrap_or_default().into());
        Ok(json!({"success":true,"output":"triggered","retryable":false}))
    }
}

pub(super) async fn connections() -> (Connection, Connection, Arc<State>) {
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
pub(super) fn callback() -> CallbackAddress {
    CallbackAddress {
        destination: "test.ScheduleHost".into(),
        object_path: "/test/ScheduleHost".into(),
    }
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
async fn run_job_now_is_a_typed_module_owned_admission_operation() {
    use tinyflows_bus::schedule::{ManualDelivery, RunJobNow, RunJobReply, StoreConfig};
    use tinystoragedrivers_core::{MemoryStorage, Scope, StorageBackend};

    let (_server, host, state) = connections().await;
    let backend = MemoryStorage::new();
    let scoped = backend
        .for_scope(&Scope::new("manual-admission-scope").unwrap())
        .unwrap();
    let documents = tinyflows_drivers::schedule::CronDocuments::new(scoped.documents().clone());
    let now = chrono::Utc::now();
    let job = documents
        .add_shell_job(
            Some("manual-job".into()),
            Schedule::At {
                at: now + chrono::Duration::hours(1),
            },
            "echo manual",
        )
        .await
        .unwrap();
    let job = documents
        .update_job(
            &job.id,
            tinyflows_schedule::CronJobPatch {
                enabled: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    host.serve_at(
        "/test/Documents".try_into().unwrap(),
        document_callbacks::Documents {
            docs: scoped.documents().clone(),
            conflict_once: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            atomic_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        },
    )
    .await
    .unwrap();
    let storage: StoreConfig = serde_json::from_value(json!({
        "backend": {"kind":"documents", "callback": {
            "destination":"test.ScheduleHost", "object_path":"/test/Documents"
        }}
    }))
    .unwrap();
    let proxy = host
        .proxy(
            schedule::BUS_NAME,
            schedule::OBJECT_PATH,
            schedule::INTERFACE,
        )
        .unwrap();
    let result: RunJobReply = proxy
        .call(
            "RunJobNow",
            &(RunJobNow {
                scope: "manual-admission-scope".into(),
                callback: callback(),
                config: json!({}),
                storage,
                job: job.clone(),
                run_id: "manual-run".into(),
                delivery: ManualDelivery::NotRequested,
                wait: true,
            },),
        )
        .await
        .unwrap();
    let RunJobReply::Completed {
        run_id,
        success,
        delivery_status,
        ..
    } = result
    else {
        panic!("blocking Run Now should return a terminal result")
    };
    assert_eq!(run_id, "manual-run");
    assert!(success);
    assert_eq!(delivery_status, None);
    assert_eq!(*state.executions.lock().await, ["manual-run"]);
    let after = documents.get_job(&job.id).await.unwrap();
    assert_eq!(after.next_run, job.next_run);
    assert!(!after.enabled);
    assert_eq!(after.delete_after_run, job.delete_after_run);
    let history = documents.list_runs(&job.id, 10).await.unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].status, "ok");
    assert_eq!(history[0].delivery_status, None);
}

#[tokio::test]
async fn run_job_now_returns_typed_refusal_when_a_scheduled_claim_is_active() {
    use chrono::Duration as ChronoDuration;
    use tinyflows_bus::schedule::{
        ManualDelivery, RunJobNow, RunJobRefusal, RunJobReply, StoreConfig,
    };
    use tinystoragedrivers_core::{MemoryStorage, Scope, StorageBackend};

    let (_server, host, _) = connections().await;
    let backend = MemoryStorage::new();
    let scoped = backend
        .for_scope(&Scope::new("held-scheduled-run").unwrap())
        .unwrap();
    let documents = tinyflows_drivers::schedule::CronDocuments::new(scoped.documents().clone());
    let now = chrono::Utc::now();
    let job = documents
        .add_shell_job(
            None,
            Schedule::At {
                at: now + ChronoDuration::hours(1),
            },
            "echo held",
        )
        .await
        .unwrap();
    let active = documents
        .claim_job_now(&job, "scheduled-owner", now, ChronoDuration::hours(2))
        .await
        .unwrap()
        .unwrap();
    host.serve_at(
        "/test/Documents".try_into().unwrap(),
        document_callbacks::Documents {
            docs: scoped.documents().clone(),
            conflict_once: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            atomic_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        },
    )
    .await
    .unwrap();
    let storage: StoreConfig = serde_json::from_value(json!({
        "backend": {"kind":"documents", "callback": {
            "destination":"test.ScheduleHost", "object_path":"/test/Documents"
        }}
    }))
    .unwrap();
    let proxy = host
        .proxy(
            schedule::BUS_NAME,
            schedule::OBJECT_PATH,
            schedule::INTERFACE,
        )
        .unwrap();
    let reply: RunJobReply = proxy
        .call(
            "RunJobNow",
            &(RunJobNow {
                scope: "held-scheduled-run".into(),
                callback: callback(),
                config: json!({}),
                storage,
                job: job.clone(),
                run_id: "refused-run".into(),
                delivery: ManualDelivery::NotRequested,
                wait: true,
            },),
        )
        .await
        .unwrap();
    assert!(matches!(
        reply,
        RunJobReply::Refused {
            reason: RunJobRefusal::AlreadyRunning
        }
    ));
    assert!(documents.list_runs(&job.id, 10).await.unwrap().is_empty());
    assert!(
        documents
            .release_claim(&active, chrono::Utc::now())
            .await
            .unwrap()
    );
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
    let competing = StartScheduler {
        scope: "manual-scope".into(),
        callback: callback(),
        config: json!({}),
        manual_ticks: false,
        storage: None,
    };
    assert!(
        proxy
            .call::<String>("StartScheduler", &(competing,))
            .await
            .is_err()
    );
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
    let restarted: String = proxy
        .call(
            "StartScheduler",
            &(StartScheduler {
                scope: "manual-scope".into(),
                callback: callback(),
                config: json!({}),
                manual_ticks: true,
                storage: None,
            },),
        )
        .await
        .unwrap();
    proxy
        .call::<()>("StopScheduler", &(&restarted,))
        .await
        .unwrap();
}

#[tokio::test]
async fn module_job_storage_preserves_policy_updates_and_run_history() {
    let (_server, host, _) = connections().await;
    let proxy = host
        .proxy(
            schedule::BUS_NAME,
            schedule::OBJECT_PATH,
            schedule::INTERFACE,
        )
        .unwrap();
    host.serve_at(
        "/test/Documents".try_into().unwrap(),
        document_callbacks::Documents {
            docs: {
                use tinystoragedrivers_core::StorageBackend;
                tinystoragedrivers_core::MemoryStorage::new()
                    .for_scope(&tinystoragedrivers_core::Scope::local())
                    .unwrap()
                    .documents()
                    .clone()
            },
            conflict_once: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            atomic_calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        },
    )
    .await
    .unwrap();
    let storage = json!({"backend":{"kind":"documents","callback":{"destination":"test.ScheduleHost","object_path":"/test/Documents"}},"max_run_history":2,"max_tasks":5});
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
pub(super) mod document_callbacks;

#[cfg(test)]
#[path = "schedule_queue_tests.rs"]
mod queue_tests;

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
    let stored = docs.get("cron.jobs", id).await.unwrap().unwrap();
    let mut body = stored.doc.clone();
    body.as_object_mut().unwrap().remove("policy");
    docs.put("cron.jobs", id, body, stored.unchanged())
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

#[cfg(test)]
#[path = "schedule_manual_worker_service_tests.rs"]
mod manual_worker_tests;
