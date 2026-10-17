use super::*;

#[tokio::test]
async fn prepared_manual_worker_stop_wait_and_ack_are_idempotent_and_fenced() {
    use tinyflows_bus::schedule::{
        ManualDelivery, ManualRunOutcome, ManualRunRequest, ManualRunState, PrepareManualRun,
        StartManualRun, StopManualRun, StoreConfig, WaitManualRun,
    };
    use tinystoragedrivers_core::{MemoryStorage, Scope, StorageBackend};

    let (_server, host, state) = connections().await;
    state
        .defer_execute
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let backend = MemoryStorage::new();
    let scoped = backend
        .for_scope(&Scope::new("manual-worker-lifecycle").unwrap())
        .unwrap();
    let documents = tinyflows_drivers::schedule::CronDocuments::new(scoped.documents().clone());
    let now = chrono::Utc::now();
    let job = documents
        .add_shell_job(
            Some("manual-worker-job".into()),
            Schedule::At {
                at: now + chrono::Duration::hours(1),
            },
            "echo lifecycle",
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
    let prepare = PrepareManualRun {
        scope: "manual-worker-lifecycle".into(),
        request_id: "request-1".into(),
    };
    let handle: tinyflows_bus::schedule::ManualRunHandle =
        proxy.call("PrepareManualRun", &prepare).await.unwrap();
    let repeated: tinyflows_bus::schedule::ManualRunHandle =
        proxy.call("PrepareManualRun", &prepare).await.unwrap();
    assert_eq!(handle, repeated);

    let start = StartManualRun {
        handle: handle.clone(),
        request: ManualRunRequest {
            callback: callback(),
            config: json!({"retries":3,"backoff_ms":1}),
            storage: storage.clone(),
            job: job.clone(),
            run_id: "manual-worker-run".into(),
            delivery: ManualDelivery::NotRequested,
        },
    };
    let first_state: ManualRunState = proxy.call("StartManualRun", &start).await.unwrap();
    assert!(matches!(
        first_state,
        ManualRunState::Starting | ManualRunState::Running { .. }
    ));
    let waiter = tokio::spawn({
        let proxy = proxy.clone();
        let request = WaitManualRun {
            handle: handle.clone(),
        };
        async move {
            proxy
                .call::<Option<ManualRunState>>("WaitManualRun", &request)
                .await
        }
    });
    waiter.abort();
    state.execute_started.notified().await;

    let repeated_start: ManualRunState = proxy.call("StartManualRun", &start).await.unwrap();
    assert!(matches!(
        repeated_start,
        ManualRunState::Running { .. } | ManualRunState::Stopping { .. }
    ));
    assert_eq!(*state.executions.lock().await, ["manual-worker-run"]);
    let stopping: Option<ManualRunState> = proxy
        .call(
            "StopManualRun",
            &(StopManualRun {
                handle: handle.clone(),
            },),
        )
        .await
        .unwrap();
    assert!(matches!(stopping, Some(ManualRunState::Stopping { .. })));
    state.execute_release.notify_one();

    let mut terminal = None;
    for _ in 0..10_000 {
        let current: ManualRunState = proxy
            .call::<Option<ManualRunState>>(
                "WaitManualRun",
                &(WaitManualRun {
                    handle: handle.clone(),
                },),
            )
            .await
            .unwrap()
            .expect("known live handle remains observable");
        if matches!(current, ManualRunState::Terminal { .. }) {
            terminal = Some(current);
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(matches!(
        terminal,
        Some(ManualRunState::Terminal {
            outcome: ManualRunOutcome::Stopped { .. }
        })
    ));
    let acknowledged: bool = proxy
        .call(
            "AcknowledgeManualRun",
            &(tinyflows_bus::schedule::AcknowledgeManualRun {
                handle: handle.clone(),
            },),
        )
        .await
        .unwrap();
    assert!(acknowledged);
    assert!(
        proxy
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
        proxy
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
    let late_start = proxy.call::<ManualRunState>("StartManualRun", &start).await;
    assert!(
        late_start.is_err(),
        "late start cannot recreate acknowledged state"
    );

    assert!(
        documents.list_runs(&job.id, 10).await.unwrap().is_empty(),
        "confirmed claim release removes the queued placeholder"
    );
    let released = documents
        .claim_job_now(
            &job,
            "verify-release",
            chrono::Utc::now(),
            chrono::Duration::minutes(1),
        )
        .await
        .unwrap();
    let Some(released) = released else {
        panic!("terminal Stopped must follow confirmed claim release")
    };
    assert!(
        documents
            .release_claim(&released, chrono::Utc::now())
            .await
            .unwrap()
    );

    let after_ack: tinyflows_bus::schedule::ManualRunHandle =
        proxy.call("PrepareManualRun", &prepare).await.unwrap();
    assert_ne!(handle, after_ack);

    state
        .defer_execute
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let completed_handle: tinyflows_bus::schedule::ManualRunHandle = proxy
        .call(
            "PrepareManualRun",
            &(PrepareManualRun {
                scope: "manual-worker-lifecycle".into(),
                request_id: "request-2".into(),
            },),
        )
        .await
        .unwrap();
    let completed_start = StartManualRun {
        handle: completed_handle.clone(),
        request: ManualRunRequest {
            callback: callback(),
            config: json!({}),
            storage,
            job: job.clone(),
            run_id: "manual-worker-completed".into(),
            delivery: ManualDelivery::NotRequested,
        },
    };
    proxy
        .call::<ManualRunState>("StartManualRun", &completed_start)
        .await
        .unwrap();
    let mut completed = None;
    for _ in 0..10_000 {
        let current: ManualRunState = proxy
            .call::<Option<ManualRunState>>(
                "WaitManualRun",
                &(WaitManualRun {
                    handle: completed_handle.clone(),
                },),
            )
            .await
            .unwrap()
            .expect("known live handle remains observable");
        if matches!(current, ManualRunState::Terminal { .. }) {
            completed = Some(current);
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(matches!(
        completed,
        Some(ManualRunState::Terminal {
            outcome: ManualRunOutcome::Completed {
                run_id,
                success: true,
                ..
            }
        }) if run_id == "manual-worker-completed"
    ));
    let history = documents.list_runs(&job.id, 10).await.unwrap();
    assert_eq!(history[0].status, "ok");
    assert_eq!(history[0].output.as_deref(), Some("executed"));
    assert!(
        proxy
            .call::<bool>(
                "AcknowledgeManualRun",
                &(tinyflows_bus::schedule::AcknowledgeManualRun {
                    handle: completed_handle,
                },),
            )
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn delivery_projection_error_releases_claim_before_failed_terminal() {
    use tinyflows_bus::schedule::{
        ManualDelivery, ManualRunOutcome, ManualRunRequest, ManualRunState, PrepareManualRun,
        StartManualRun, WaitManualRun,
    };
    use tinystoragedrivers_core::{MemoryStorage, Scope, StorageBackend};

    let (_server, host, state) = connections().await;
    state
        .fail_deliver
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let scoped = MemoryStorage::new()
        .for_scope(&Scope::new("manual-delivery-failure").unwrap())
        .unwrap();
    let documents = tinyflows_drivers::schedule::CronDocuments::new(scoped.documents().clone());
    let job = documents
        .add_shell_job(
            None,
            Schedule::At {
                at: chrono::Utc::now() + chrono::Duration::hours(1),
            },
            "echo delivery failure",
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
    let proxy = host
        .proxy(
            schedule::BUS_NAME,
            schedule::OBJECT_PATH,
            schedule::INTERFACE,
        )
        .unwrap();
    let handle: tinyflows_bus::schedule::ManualRunHandle = proxy
        .call(
            "PrepareManualRun",
            &(PrepareManualRun {
                scope: "manual-delivery-failure".into(),
                request_id: "deliver-fails".into(),
            },),
        )
        .await
        .unwrap();
    proxy
        .call::<ManualRunState>(
            "StartManualRun",
            &(StartManualRun {
                handle: handle.clone(),
                request: ManualRunRequest {
                    callback: callback(),
                    config: json!({}),
                    storage: serde_json::from_value(json!({
                        "backend":{"kind":"documents","callback":{
                            "destination":"test.ScheduleHost","object_path":"/test/Documents"
                        }}
                    }))
                    .unwrap(),
                    job: job.clone(),
                    run_id: "deliver-fails-run".into(),
                    delivery: ManualDelivery::NotRequested,
                },
            },),
        )
        .await
        .unwrap();
    let mut terminal = None;
    for _ in 0..10_000 {
        let current: ManualRunState = proxy
            .call::<Option<ManualRunState>>(
                "WaitManualRun",
                &(WaitManualRun {
                    handle: handle.clone(),
                },),
            )
            .await
            .unwrap()
            .expect("known live handle remains observable");
        if matches!(current, ManualRunState::Terminal { .. }) {
            terminal = Some(current);
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(matches!(
        terminal,
        Some(ManualRunState::Terminal {
            outcome: ManualRunOutcome::Failed { .. }
        })
    ));
    assert!(documents.list_runs(&job.id, 10).await.unwrap().is_empty());
    let released = documents
        .claim_job_now(
            &job,
            "verify-delivery-release",
            chrono::Utc::now(),
            chrono::Duration::minutes(1),
        )
        .await
        .unwrap()
        .expect("Failed is terminal only after cleanup releases the claim");
    assert!(
        documents
            .release_claim(&released, chrono::Utc::now())
            .await
            .unwrap()
    );
    assert_eq!(*state.executions.lock().await, ["deliver-fails-run"]);
}
