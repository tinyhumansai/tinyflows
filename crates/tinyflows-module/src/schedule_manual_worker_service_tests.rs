use super::*;

#[cfg(test)]
#[path = "schedule_manual_worker_lifecycle_service_tests.rs"]
mod lifecycle_tests;

#[cfg(test)]
#[path = "schedule_manual_worker_identity_tests.rs"]
mod identity_tests;

#[tokio::test]
async fn claim_reply_lost_after_commit_is_reconciled_and_released_before_terminal() {
    use tinyflows_bus::schedule::{
        ManualDelivery, ManualRunOutcome, ManualRunRequest, ManualRunState, PrepareManualRun,
        StartManualRun, WaitManualRun,
    };
    use tinystoragedrivers_core::{MemoryStorage, Scope, StorageBackend};

    let (_server, host, state) = connections().await;
    let scoped = MemoryStorage::new()
        .for_scope(&Scope::new("manual-claim-reconcile").unwrap())
        .unwrap();
    let documents = tinyflows_drivers::schedule::CronDocuments::new(scoped.documents().clone());
    let job = documents
        .add_shell_job(
            None,
            Schedule::At {
                at: chrono::Utc::now() + chrono::Duration::hours(1),
            },
            "echo reconcile",
        )
        .await
        .unwrap();
    host.serve_at(
        "/test/FaultyDocuments".try_into().unwrap(),
        document_callbacks::CommitThenFail {
            docs: scoped.documents().clone(),
            fail_claim_reply: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            fail_release_reply: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            fail_completion_before_commit: Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
                scope: "manual-claim-reconcile".into(),
                request_id: "lost-claim-reply".into(),
            },),
        )
        .await
        .unwrap();
    let start = StartManualRun {
        handle: handle.clone(),
        request: ManualRunRequest {
            callback: callback(),
            config: json!({}),
            storage: serde_json::from_value(json!({
                "backend":{"kind":"documents","callback":{
                    "destination":"test.ScheduleHost","object_path":"/test/FaultyDocuments"
                }}
            }))
            .unwrap(),
            job: job.clone(),
            run_id: "lost-claim-run".into(),
            delivery: ManualDelivery::NotRequested,
        },
    };
    proxy
        .call::<ManualRunState>("StartManualRun", &start)
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
    assert!(state.executions.lock().await.is_empty());
    assert!(documents.list_runs(&job.id, 10).await.unwrap().is_empty());
    let fresh_job = documents.get_job(&job.id).await.unwrap();
    let released = documents
        .claim_job_now(
            &fresh_job,
            "verify-reconciled-release",
            chrono::Utc::now(),
            chrono::Duration::minutes(1),
        )
        .await
        .unwrap()
        .expect("claim response loss must be reconciled and released before terminal");
    assert!(
        documents
            .release_claim(&released, chrono::Utc::now())
            .await
            .unwrap()
    );
}

#[tokio::test(start_paused = true)]
async fn release_reply_loss_reports_cleanup_pending_until_retry_confirms_fence() {
    use tinyflows_bus::schedule::{
        ManualDelivery, ManualRunOutcome, ManualRunRequest, ManualRunState, PrepareManualRun,
        StartManualRun, StopManualRun, WaitManualRun,
    };
    use tinystoragedrivers_core::{MemoryStorage, Scope, StorageBackend};

    let (_server, host, state) = connections().await;
    state
        .defer_execute
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let scoped = MemoryStorage::new()
        .for_scope(&Scope::new("manual-release-reconcile").unwrap())
        .unwrap();
    let documents = tinyflows_drivers::schedule::CronDocuments::new(scoped.documents().clone());
    let job = documents
        .add_shell_job(
            None,
            Schedule::At {
                at: chrono::Utc::now() + chrono::Duration::hours(1),
            },
            "echo release",
        )
        .await
        .unwrap();
    let fail_release = Arc::new(std::sync::atomic::AtomicBool::new(false));
    host.serve_at(
        "/test/FaultyDocuments".try_into().unwrap(),
        document_callbacks::CommitThenFail {
            docs: scoped.documents().clone(),
            fail_claim_reply: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            fail_release_reply: fail_release.clone(),
            fail_completion_before_commit: Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
                scope: "manual-release-reconcile".into(),
                request_id: "lost-release-reply".into(),
            },),
        )
        .await
        .unwrap();
    let start = StartManualRun {
        handle: handle.clone(),
        request: ManualRunRequest {
            callback: callback(),
            config: json!({}),
            storage: serde_json::from_value(json!({
                "backend":{"kind":"documents","callback":{
                    "destination":"test.ScheduleHost","object_path":"/test/FaultyDocuments"
                }}
            }))
            .unwrap(),
            job: job.clone(),
            run_id: "lost-release-run".into(),
            delivery: ManualDelivery::NotRequested,
        },
    };
    proxy
        .call::<ManualRunState>("StartManualRun", &start)
        .await
        .unwrap();
    state.execute_started.notified().await;
    fail_release.store(true, std::sync::atomic::Ordering::SeqCst);
    proxy
        .call::<Option<ManualRunState>>(
            "StopManualRun",
            &(StopManualRun {
                handle: handle.clone(),
            },),
        )
        .await
        .unwrap();
    state.execute_release.notify_one();

    let mut cleanup_pending = false;
    for _ in 0..10_000 {
        if matches!(
            proxy
                .call::<Option<ManualRunState>>(
                    "WaitManualRun",
                    &(WaitManualRun {
                        handle: handle.clone(),
                    },),
                )
                .await
                .unwrap(),
            Some(ManualRunState::CleanupPending { .. })
        ) {
            cleanup_pending = true;
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(
        cleanup_pending,
        "lost release reply must not publish terminal state"
    );
    tokio::time::advance(std::time::Duration::from_millis(250)).await;

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
}

#[tokio::test]
async fn unsupported_claim_backend_fails_before_recording_claim_intent() {
    use tinyflows_bus::schedule::{
        ManualDelivery, ManualRunOutcome, ManualRunRequest, ManualRunState, PrepareManualRun,
        StartManualRun, WaitManualRun,
    };
    use tinystoragedrivers_core::{MemoryStorage, Scope, StorageBackend};

    let (_server, host, state) = connections().await;
    let scoped = MemoryStorage::new()
        .for_scope(&Scope::new("manual-no-transactions").unwrap())
        .unwrap();
    let documents = tinyflows_drivers::schedule::CronDocuments::new(scoped.documents().clone());
    let job = documents
        .add_shell_job(
            None,
            Schedule::At {
                at: chrono::Utc::now() + chrono::Duration::hours(1),
            },
            "echo unsupported",
        )
        .await
        .unwrap();
    host.serve_at(
        "/test/NoTransactions".try_into().unwrap(),
        document_callbacks::NoTransactions {
            docs: scoped.documents().clone(),
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
                scope: "manual-no-transactions".into(),
                request_id: "unsupported-backend".into(),
            },),
        )
        .await
        .unwrap();
    let start = StartManualRun {
        handle: handle.clone(),
        request: ManualRunRequest {
            callback: callback(),
            config: json!({}),
            storage: serde_json::from_value(json!({
                "backend":{"kind":"documents","callback":{
                    "destination":"test.ScheduleHost","object_path":"/test/NoTransactions"
                }}
            }))
            .unwrap(),
            job: job.clone(),
            run_id: "unsupported-run".into(),
            delivery: ManualDelivery::NotRequested,
        },
    };
    proxy
        .call::<ManualRunState>("StartManualRun", &start)
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
    assert!(state.executions.lock().await.is_empty());
    assert!(documents.list_runs(&job.id, 10).await.unwrap().is_empty());
    let admitted = documents
        .claim_job_now(
            &job,
            "verify-no-claim",
            chrono::Utc::now(),
            chrono::Duration::minutes(1),
        )
        .await
        .unwrap();
    let Some(admitted) = admitted else {
        panic!("preflight failure leaves no held claim")
    };
    assert!(
        documents
            .release_claim(&admitted, chrono::Utc::now())
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn terminal_commit_error_releases_queued_claim_before_failed_terminal() {
    use tinyflows_bus::schedule::{
        ManualDelivery, ManualRunOutcome, ManualRunRequest, ManualRunState, PrepareManualRun,
        StartManualRun, WaitManualRun,
    };
    use tinystoragedrivers_core::{MemoryStorage, Scope, StorageBackend};

    let (_server, host, state) = connections().await;
    let scoped = MemoryStorage::new()
        .for_scope(&Scope::new("manual-completion-failure").unwrap())
        .unwrap();
    let documents = tinyflows_drivers::schedule::CronDocuments::new(scoped.documents().clone());
    let job = documents
        .add_shell_job(
            None,
            Schedule::At {
                at: chrono::Utc::now() + chrono::Duration::hours(1),
            },
            "echo completion failure",
        )
        .await
        .unwrap();
    host.serve_at(
        "/test/FaultyDocuments".try_into().unwrap(),
        document_callbacks::CommitThenFail {
            docs: scoped.documents().clone(),
            fail_claim_reply: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            fail_release_reply: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            fail_completion_before_commit: Arc::new(std::sync::atomic::AtomicBool::new(true)),
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
                scope: "manual-completion-failure".into(),
                request_id: "completion-fails".into(),
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
                            "destination":"test.ScheduleHost","object_path":"/test/FaultyDocuments"
                        }}
                    }))
                    .unwrap(),
                    job: job.clone(),
                    run_id: "completion-fails-run".into(),
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
    assert_eq!(*state.executions.lock().await, ["completion-fails-run"]);
    assert!(documents.list_runs(&job.id, 10).await.unwrap().is_empty());
    let released = documents
        .claim_job_now(
            &job,
            "verify-completion-release",
            chrono::Utc::now(),
            chrono::Duration::minutes(1),
        )
        .await
        .unwrap()
        .expect("failed completion must release its occurrence before terminal");
    assert!(
        documents
            .release_claim(&released, chrono::Utc::now())
            .await
            .unwrap()
    );
}
