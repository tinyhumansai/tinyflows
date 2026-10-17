use super::{callbacks, document_callbacks};
use serde_json::json;
use std::sync::Arc;
use tinybus::{Connection, broker::Broker, module::ModuleHost, transport::memory::MemoryTransport};
use tinyflows_bus::schedule::{
    AcknowledgeManualRun, ManualDelivery, ManualRunOutcome, ManualRunRequest, ManualRunState,
    PrepareManualRun, StartManualRun, StopManualRun, StoreConfig, WaitManualRun,
};
use tinyflows_drivers::schedule::CronDocuments;
use tinyflows_schedule::Schedule;
use tinystoragedrivers_core::{MemoryStorage, Scope, StorageBackend};

#[tokio::test]
#[ignore = "run with TINYFLOWS_MODULE_ARTIFACT set to a compiled module"]
async fn compiled_module_serves_manual_worker_lifecycle_through_schedule_abi() {
    let artifact = std::env::var_os("TINYFLOWS_MODULE_ARTIFACT")
        .expect("set TINYFLOWS_MODULE_ARTIFACT to a compiled cdylib");
    let broker = Broker::new();
    let host = ModuleHost::new(broker.clone());
    host.load_file(std::path::PathBuf::from(artifact)).unwrap();
    let (client, transport) = MemoryTransport::pair();
    broker.attach(Arc::new(transport));
    let connection = Connection::connect(Box::new(client)).await.unwrap();

    let callback_state = Arc::new(callbacks::State::default());
    callback_state
        .defer_execute
        .store(true, std::sync::atomic::Ordering::SeqCst);
    connection
        .serve_at(
            "/test/ScheduleCallbacks".try_into().unwrap(),
            callbacks::Callbacks(callback_state.clone()),
        )
        .await
        .unwrap();
    connection
        .request_name("test.ScheduleCallbacks")
        .await
        .unwrap();

    let scoped = MemoryStorage::new().for_scope(&Scope::local()).unwrap();
    let documents = CronDocuments::new(scoped.documents().clone());
    let job = documents
        .add_shell_job(
            Some("artifact-manual-lifecycle".into()),
            Schedule::At {
                at: chrono::Utc::now() + chrono::Duration::hours(1),
            },
            "echo lifecycle",
        )
        .await
        .unwrap();
    connection
        .serve_at(
            "/test/Documents".try_into().unwrap(),
            document_callbacks::Documents {
                docs: scoped.documents().clone(),
                conflict_once: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                atomic_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            },
        )
        .await
        .unwrap();

    let schedule = connection
        .proxy(
            tinyflows_bus::schedule::BUS_NAME,
            tinyflows_bus::schedule::OBJECT_PATH,
            tinyflows_bus::schedule::INTERFACE,
        )
        .unwrap();
    let storage: StoreConfig = serde_json::from_value(json!({
        "backend":{"kind":"documents","callback":{
            "destination":"test.ScheduleCallbacks","object_path":"/test/Documents"
        }}
    }))
    .unwrap();
    let prepare = PrepareManualRun {
        scope: "artifact-manual-scope".into(),
        request_id: "artifact-manual-request".into(),
    };
    let handle: tinyflows_bus::schedule::ManualRunHandle =
        schedule.call("PrepareManualRun", &prepare).await.unwrap();
    let repeated: tinyflows_bus::schedule::ManualRunHandle =
        schedule.call("PrepareManualRun", &prepare).await.unwrap();
    assert_eq!(handle, repeated);
    let start = StartManualRun {
        handle: handle.clone(),
        request: ManualRunRequest {
            callback: tinyflows_bus::schedule::CallbackAddress {
                destination: "test.ScheduleCallbacks".into(),
                object_path: "/test/ScheduleCallbacks".into(),
            },
            config: json!({"retries":3,"backoff_ms":1}),
            storage: storage.clone(),
            job: job.clone(),
            run_id: "artifact-manual-run".into(),
            delivery: ManualDelivery::NotRequested,
        },
    };
    let first: ManualRunState = schedule.call("StartManualRun", &start).await.unwrap();
    assert!(matches!(
        first,
        ManualRunState::Starting | ManualRunState::Running { .. }
    ));
    let canceled_waiter = tokio::spawn({
        let schedule = schedule.clone();
        let request = WaitManualRun {
            handle: handle.clone(),
        };
        async move {
            schedule
                .call::<Option<ManualRunState>>("WaitManualRun", &request)
                .await
        }
    });
    canceled_waiter.abort();
    callback_state.execute_started.notified().await;
    assert_eq!(
        documents.list_runs(&job.id, 10).await.unwrap()[0].status,
        "queued",
        "Running is reported only after durable queue admission"
    );

    let retry_after_lost_reply: ManualRunState =
        schedule.call("StartManualRun", &start).await.unwrap();
    assert!(matches!(
        retry_after_lost_reply,
        ManualRunState::Running { .. } | ManualRunState::Stopping { .. }
    ));
    let stopping: Option<ManualRunState> = schedule
        .call(
            "StopManualRun",
            &(StopManualRun {
                handle: handle.clone(),
            },),
        )
        .await
        .unwrap();
    assert!(matches!(stopping, Some(ManualRunState::Stopping { .. })));
    callback_state.execute_release.notify_one();
    let stopped = wait_terminal(&schedule, &handle).await;
    assert!(matches!(
        stopped,
        ManualRunState::Terminal {
            outcome: ManualRunOutcome::Stopped { .. }
        }
    ));
    assert_eq!(
        callback_state.executions.lock().unwrap().as_slice(),
        ["artifact-manual-run"]
    );
    assert!(documents.list_runs(&job.id, 10).await.unwrap().is_empty());
    let released = documents
        .claim_job_now(
            &job,
            "artifact-verify-release",
            chrono::Utc::now(),
            chrono::Duration::minutes(1),
        )
        .await
        .unwrap()
        .expect("Stopped is terminal only after confirmed claim release");
    assert!(
        documents
            .release_claim(&released, chrono::Utc::now())
            .await
            .unwrap()
    );
    assert!(
        schedule
            .call::<bool>(
                "AcknowledgeManualRun",
                &(AcknowledgeManualRun {
                    handle: handle.clone(),
                },),
            )
            .await
            .unwrap()
    );
    assert!(
        schedule
            .call::<Option<ManualRunState>>(
                "WaitManualRun",
                &(WaitManualRun {
                    handle: handle.clone(),
                },),
            )
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        schedule
            .call::<Option<ManualRunState>>(
                "StopManualRun",
                &(StopManualRun {
                    handle: handle.clone(),
                },),
            )
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        schedule
            .call::<ManualRunState>("StartManualRun", &start)
            .await
            .is_err()
    );

    let renewed: tinyflows_bus::schedule::ManualRunHandle =
        schedule.call("PrepareManualRun", &prepare).await.unwrap();
    assert_ne!(renewed.token, handle.token);
    assert!(
        schedule
            .call::<ManualRunState>("StartManualRun", &start)
            .await
            .is_err(),
        "fresh request-id generation cannot revive the old handle"
    );
    callback_state
        .defer_execute
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let completed_handle: tinyflows_bus::schedule::ManualRunHandle = schedule
        .call::<tinyflows_bus::schedule::ManualRunHandle>(
            "PrepareManualRun",
            &(PrepareManualRun {
                scope: "artifact-manual-scope".into(),
                request_id: "artifact-completed-request".into(),
            },),
        )
        .await
        .unwrap();
    let completed_start = StartManualRun {
        handle: completed_handle.clone(),
        request: ManualRunRequest {
            run_id: "artifact-completed-run".into(),
            ..start.request.clone()
        },
    };
    schedule
        .call::<ManualRunState>("StartManualRun", &completed_start)
        .await
        .unwrap();
    let completed = wait_terminal(&schedule, &completed_handle).await;
    assert!(matches!(
        completed,
        ManualRunState::Terminal {
            outcome: ManualRunOutcome::Completed {
                run_id,
                success: true,
                ..
            }
        } if run_id == "artifact-completed-run"
    ));
    let history = documents.list_runs(&job.id, 10).await.unwrap();
    assert_eq!(history[0].status, "ok");
    assert!(
        schedule
            .call::<bool>(
                "AcknowledgeManualRun",
                &(AcknowledgeManualRun {
                    handle: completed_handle,
                },),
            )
            .await
            .unwrap()
    );
}

async fn wait_terminal(
    schedule: &tinybus::Proxy,
    handle: &tinyflows_bus::schedule::ManualRunHandle,
) -> ManualRunState {
    for _ in 0..10_000 {
        let Some(state) = schedule
            .call::<Option<ManualRunState>>(
                "WaitManualRun",
                &(WaitManualRun {
                    handle: handle.clone(),
                },),
            )
            .await
            .unwrap()
        else {
            panic!("live manual worker handle unexpectedly became absent")
        };
        if matches!(state, ManualRunState::Terminal { .. }) {
            return state;
        }
        tokio::task::yield_now().await;
    }
    panic!("manual worker did not reach a terminal state")
}
