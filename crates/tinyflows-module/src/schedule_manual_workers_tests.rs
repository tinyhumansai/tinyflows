use super::*;

fn prepare(scope: &str, request_id: &str) -> PrepareManualRun {
    PrepareManualRun {
        scope: scope.into(),
        request_id: request_id.into(),
    }
}

#[test]
fn prepare_is_idempotent_per_scope_and_acknowledged_handles_are_never_reused() {
    let mut workers = ManualWorkers::default();
    let first = workers.prepare(prepare("profile-a", "request-1"));
    let repeated = workers.prepare(prepare("profile-a", "request-1"));
    let other_scope = workers.prepare(prepare("profile-b", "request-1"));
    assert_eq!(first, repeated);
    assert_ne!(first, other_scope);

    let reservation = workers.get(&first).unwrap();
    assert!(matches!(
        reservation.entry.request_stop(),
        ManualRunState::Terminal { .. }
    ));
    {
        let mut control = reservation.entry.control.lock().unwrap();
        control.expires_at = Instant::now() - Duration::from_secs(24 * 60 * 60);
    }
    assert!(
        workers.get(&first).is_some(),
        "terminal results persist until Ack"
    );
    assert!(workers.acknowledge(&first));
    assert!(workers.get(&first).is_none());

    let after_ack = workers.prepare(prepare("profile-a", "request-1"));
    assert_ne!(first, after_ack);
    assert!(
        workers.get(&first).is_none(),
        "stale Start cannot recover an acknowledged handle"
    );
}

#[test]
fn duplicate_start_is_idempotent_but_a_changed_binding_is_refused() {
    let mut workers = ManualWorkers::default();
    let handle = workers.prepare(prepare("profile-a", "request-2"));
    let entry = workers.get(&handle).unwrap().entry;
    let binding = serde_json::json!({"callback":"agent-a","store":"store-a"});
    assert!(entry.begin_start(binding.clone()).unwrap());
    assert!(!entry.begin_start(binding).unwrap());
    assert!(
        entry
            .begin_start(serde_json::json!({"callback":"agent-b","store":"store-a"}))
            .is_err()
    );
    assert!(matches!(
        entry.request_stop(),
        ManualRunState::Stopping { .. }
    ));
    assert!(
        !workers.acknowledge(&handle),
        "live workers cannot be acknowledged"
    );
}

#[test]
fn active_worker_state_is_not_expired_by_the_inert_reservation_ttl() {
    let mut workers = ManualWorkers::default();
    let handle = workers.prepare(prepare("profile-a", "request-3"));
    let entry = workers.get(&handle).unwrap().entry;
    entry
        .begin_start(serde_json::json!({"job":"job-a"}))
        .unwrap();
    {
        let mut control = entry.control.lock().unwrap();
        control.expires_at = Instant::now() - Duration::from_secs(1);
    }
    assert!(
        workers.get(&handle).is_some(),
        "a started worker has no TTL"
    );
}

#[test]
fn stop_before_worker_subscription_is_retained_for_the_worker() {
    let mut workers = ManualWorkers::default();
    let handle = workers.prepare(prepare("profile-a", "request-stop-before-subscribe"));
    let entry = workers.get(&handle).unwrap().entry;
    entry
        .begin_start(serde_json::json!({"job":"job-a"}))
        .unwrap();

    entry.request_stop();
    let receiver = entry.stop.subscribe();
    assert!(
        *receiver.borrow(),
        "a stop signal sent before subscribe must persist"
    );
}

#[tokio::test]
async fn supervisor_releases_a_known_claim_after_worker_panic_before_terminal() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tinyflows_bus::schedule::{CronJob, OccurrenceClaim};

    struct Store(AtomicUsize);
    #[async_trait::async_trait]
    impl JobStore for Store {
        async fn due_jobs(&self, _: chrono::DateTime<chrono::Utc>) -> anyhow::Result<Vec<CronJob>> {
            Ok(vec![])
        }
        async fn release_occurrence(
            &self,
            _: &OccurrenceClaim,
            _: chrono::DateTime<chrono::Utc>,
        ) -> anyhow::Result<bool> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(true)
        }
    }
    struct TestClock;
    #[async_trait::async_trait]
    impl Clock for TestClock {
        fn now(&self) -> chrono::DateTime<chrono::Utc> {
            "2026-01-01T00:00:00Z".parse().unwrap()
        }
        async fn sleep(&self, _: Duration) {
            std::future::pending().await
        }
    }

    let job: CronJob = serde_json::from_value(serde_json::json!({
        "id":"panic-job","expression":"","schedule":{"kind":"every","every_ms":60000},
        "command":"echo panic","prompt":null,"name":null,"job_type":"shell",
        "session_target":"isolated","model":null,"agent_id":null,"enabled":true,
        "delivery":{"mode":"none"},"delete_after_run":false,
        "created_at":"2026-01-01T00:00:00Z","next_run":"2026-01-01T00:00:00Z"
    }))
    .unwrap();
    let now = "2026-01-01T00:00:00Z".parse().unwrap();
    let claim = OccurrenceClaim {
        receipt: "panic-claim".into(),
        job,
        occurrence: now,
        started_at: now,
        owner: "panic-owner".into(),
        run_id: Some("panic-run".into()),
    };
    let store = std::sync::Arc::new(Store(AtomicUsize::new(0)));
    let clock = std::sync::Arc::new(TestClock);
    let held = HeldClaim::Claimed {
        claim,
        store: store.clone(),
        clock,
    };
    let claim_slot = std::sync::Arc::new(tokio::sync::Mutex::new(Some(held)));
    let entry = std::sync::Arc::new(ManualWorkerEntry::prepared());
    entry
        .begin_start(serde_json::json!({"request":"panic"}))
        .unwrap();
    entry.set_run_id("panic-run".into());
    let worker = async {
        panic!("injected module worker panic");
        #[allow(unreachable_code)]
        Ok(ManualRunOutcome::Stopped { run_id: None })
    };

    supervise_manual_worker(entry.clone(), claim_slot, worker).await;

    assert_eq!(store.0.load(Ordering::SeqCst), 1);
    assert!(matches!(
        entry.state(),
        ManualRunState::Terminal {
            outcome: ManualRunOutcome::Failed {
                run_id: Some(run_id),
                ..
            }
        } if run_id == "panic-run"
    ));
}
