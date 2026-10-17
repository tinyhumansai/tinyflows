use super::*;
use std::future::Future;

struct ControlledLeaseClock {
    now: DateTime<Utc>,
    renewal_tick: Notify,
    waiters: AtomicUsize,
    waiting: Notify,
}
#[async_trait]
impl Clock for ControlledLeaseClock {
    fn now(&self) -> DateTime<Utc> {
        self.now
    }
    async fn sleep(&self, _: Duration) {
        std::future::pending().await
    }
    async fn lease_wait(&self, _: Duration) {
        self.waiters.fetch_add(1, Ordering::SeqCst);
        self.waiting.notify_one();
        self.renewal_tick.notified().await;
        self.waiters.fetch_sub(1, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn live_run_renews_its_durable_lease_on_a_controlled_tick() {
    let fixture = Fixture::new();
    let executor = Arc::new(BlockExecutor {
        started: Semaphore::new(0),
        release: Semaphore::new(0),
        active: AtomicUsize::new(0),
        peak: AtomicUsize::new(0),
        completed: AtomicUsize::new(0),
    });
    let clock = Arc::new(ControlledLeaseClock {
        now: fixture.now(),
        renewal_tick: Notify::new(),
        waiters: AtomicUsize::new(0),
        waiting: Notify::new(),
    });
    let mut ports = fixture.ports();
    ports.executor = executor.clone();
    ports.clock = clock.clone();
    let job = fixture.job();
    let mut scheduler = Scheduler::new(job.id.clone(), SchedulerConfig::default(), ports);
    assert_eq!(scheduler.dispatch(vec![job]).await.unwrap(), 1);
    executor.started.acquire().await.unwrap().forget();
    clock.renewal_tick.notify_one();
    fixture.renewed.notified().await;
    assert_eq!(
        fixture.renewals.load(Ordering::SeqCst),
        2,
        "one pre-attempt ownership fence plus the controlled heartbeat"
    );
    executor.release.add_permits(1);
    scheduler.drain().await.unwrap();
    assert_eq!(fixture.completions.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn queued_occurrence_renews_and_never_executes_after_takeover() {
    let fixture = Fixture::new();
    let executor = Arc::new(BlockExecutor {
        started: Semaphore::new(0),
        release: Semaphore::new(0),
        active: AtomicUsize::new(0),
        peak: AtomicUsize::new(0),
        completed: AtomicUsize::new(0),
    });
    let clock = Arc::new(ControlledLeaseClock {
        now: fixture.now(),
        renewal_tick: Notify::new(),
        waiters: AtomicUsize::new(0),
        waiting: Notify::new(),
    });
    let mut ports = fixture.ports();
    ports.executor = executor.clone();
    ports.clock = clock.clone();
    let first = fixture.job();
    let second = fixture.job();
    *fixture.lease_takeover_job.lock().unwrap() = Some(second.id.clone());
    let mut scheduler = Scheduler::new(
        uuid::Uuid::new_v4().to_string(),
        SchedulerConfig {
            max_concurrent: 1,
            ..Default::default()
        },
        ports,
    );
    assert_eq!(
        scheduler
            .dispatch(vec![first, second.clone()])
            .await
            .unwrap(),
        2
    );
    executor.started.acquire().await.unwrap().forget();
    loop {
        let waiting = clock.waiting.notified();
        if clock.waiters.load(Ordering::SeqCst) >= 2 {
            break;
        }
        waiting.await;
    }
    clock.renewal_tick.notify_waiters();
    fixture.lease_taken_over.notified().await;
    assert_eq!(executor.active.load(Ordering::SeqCst), 1);
    executor.release.add_permits(1);
    assert!(scheduler.drain().await.is_err());
    assert_eq!(executor.peak.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.records.lock().unwrap().len(), 1);
    assert_eq!(fixture.completions.lock().unwrap().len(), 1);
    assert_eq!(
        fixture.leases.lock().unwrap().get(&second.id).unwrap().0,
        "successor-owner"
    );
    scheduler.shutdown().await;
}

#[tokio::test]
async fn refill_stops_at_the_batch_cap_after_skipping_a_held_head() {
    let mut fixture = Fixture::new();
    Arc::get_mut(&mut fixture).unwrap().max_tasks = 2;
    let first = fixture.job();
    let second = fixture.job();
    let third = fixture.job();
    let fourth = fixture.job();
    *fixture.jobs.lock().unwrap() =
        vec![first.clone(), second.clone(), third.clone(), fourth.clone()];
    let held = fixture
        .claim_due_occurrence(
            &first,
            "already-running",
            fixture.now(),
            chrono::Duration::hours(1),
        )
        .await
        .unwrap()
        .unwrap();
    let mut scheduler = Scheduler::new(
        uuid::Uuid::new_v4().to_string(),
        SchedulerConfig::default(),
        fixture.ports(),
    );
    assert_eq!(scheduler.tick().await.unwrap(), 2);
    scheduler.drain().await.unwrap();
    let recorded: Vec<_> = fixture
        .records
        .lock()
        .unwrap()
        .iter()
        .map(|record| record.job_id.clone())
        .collect();
    assert_eq!(recorded.len(), 2);
    assert!(recorded.contains(&second.id));
    assert!(recorded.contains(&third.id));
    assert!(!recorded.contains(&fourth.id));
    assert!(
        fixture
            .release_occurrence(&held, fixture.now())
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn a_later_claim_error_releases_every_earlier_undispatched_receipt() {
    let fixture = Fixture::new();
    let first = fixture.job();
    let second = fixture.job();
    *fixture.jobs.lock().unwrap() = vec![first.clone(), second];
    *fixture.claim_fails_after.lock().unwrap() = Some(1);
    let mut scheduler = Scheduler::new(
        uuid::Uuid::new_v4().to_string(),
        SchedulerConfig::default(),
        fixture.ports(),
    );
    assert!(scheduler.tick().await.is_err());
    assert!(fixture.leases.lock().unwrap().is_empty());
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    assert!(fixture.records.lock().unwrap().is_empty());
}

struct CancellableExecutor {
    started: Arc<Notify>,
    release: Arc<Semaphore>,
    completed: Arc<AtomicBool>,
}
#[async_trait]
impl JobExecutor for CancellableExecutor {
    async fn execute(&self, _: &CronJob, _: &str) -> Result<Attempt> {
        self.started.notify_one();
        self.release.acquire().await.unwrap().forget();
        self.completed.store(true, Ordering::SeqCst);
        Ok(Attempt {
            success: true,
            output: "finished after takeover".into(),
            retryable: false,
            raw_detail: None,
        })
    }
}

#[tokio::test]
async fn lease_loss_waits_for_inflight_execute_then_blocks_commit() {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let fixture = Fixture::new();
        let started = Arc::new(Notify::new());
        let release = Arc::new(Semaphore::new(0));
        let completed = Arc::new(AtomicBool::new(false));
        let clock = Arc::new(ControlledLeaseClock {
            now: fixture.now(),
            renewal_tick: Notify::new(),
            waiters: AtomicUsize::new(0),
            waiting: Notify::new(),
        });
        let mut ports = fixture.ports();
        ports.executor = Arc::new(CancellableExecutor {
            started: started.clone(),
            release: release.clone(),
            completed: completed.clone(),
        });
        ports.clock = clock.clone();
        let job = fixture.job();
        let job_id = job.id.clone();
        let mut scheduler = Scheduler::new(job.id.clone(), SchedulerConfig::default(), ports);
        assert_eq!(scheduler.dispatch(vec![job]).await.unwrap(), 1);
        started.notified().await;
        loop {
            let waiting = clock.waiting.notified();
            if clock.waiters.load(Ordering::SeqCst) > 0 {
                break;
            }
            waiting.await;
        }
        fixture.take_lease_on_renew.store(true, Ordering::SeqCst);
        clock.renewal_tick.notify_one();
        fixture.lease_taken_over.notified().await;
        assert!(!completed.load(Ordering::SeqCst));
        release.add_permits(1);
        assert!(scheduler.drain().await.is_err());
        assert!(completed.load(Ordering::SeqCst));
        assert!(fixture.records.lock().unwrap().is_empty());
        assert!(fixture.projections.lock().unwrap().is_empty());
        assert_eq!(
            fixture.leases.lock().unwrap().get(&job_id).unwrap().0,
            "successor-owner"
        );
        scheduler.shutdown().await;
    })
    .await
    .expect("lease-loss test must quiesce after releasing its controlled callback");
}
#[tokio::test]
async fn dispatch_returns_before_execution_and_limits_concurrent_jobs() {
    let f = Fixture::new();
    let executor = Arc::new(BlockExecutor {
        started: Semaphore::new(0),
        release: Semaphore::new(0),
        active: AtomicUsize::new(0),
        peak: AtomicUsize::new(0),
        completed: AtomicUsize::new(0),
    });
    let mut ports = f.ports();
    ports.executor = executor.clone();
    let mut scheduler = Scheduler::new(
        uuid::Uuid::new_v4().to_string(),
        SchedulerConfig {
            max_concurrent: 1,
            ..Default::default()
        },
        ports,
    );
    assert_eq!(scheduler.dispatch(vec![f.job(), f.job()]).await.unwrap(), 2);
    executor.started.acquire().await.unwrap().forget();
    assert_eq!(executor.active.load(Ordering::SeqCst), 1);
    executor.release.add_permits(1);
    executor.started.acquire().await.unwrap().forget();
    executor.release.add_permits(1);
    scheduler.drain().await.unwrap();
    assert_eq!(executor.peak.load(Ordering::SeqCst), 1);
    assert_eq!(f.records.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn shutdown_waits_for_inflight_job_and_releases_queued_claims() {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let f = Fixture::new();
        let executor = Arc::new(BlockExecutor {
            started: Semaphore::new(0),
            release: Semaphore::new(0),
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            completed: AtomicUsize::new(0),
        });
        let mut ports = f.ports();
        ports.executor = executor.clone();
        let scope = uuid::Uuid::new_v4().to_string();
        let jobs = vec![f.job(), f.job()];
        let ids: Vec<_> = jobs.iter().map(|j| j.id.clone()).collect();
        let mut scheduler = Scheduler::new(
            scope.clone(),
            SchedulerConfig {
                max_concurrent: 1,
                ..Default::default()
            },
            ports,
        );
        scheduler.dispatch(jobs).await.unwrap();
        executor.started.acquire().await.unwrap().forget();
        assert_eq!(executor.active.load(Ordering::SeqCst), 1);
        assert!(
            ids.iter()
                .all(|id| f.leases.lock().unwrap().contains_key(id))
        );

        let (shutdown_started, shutdown_ready) = tokio::sync::oneshot::channel();
        let shutdown = tokio::spawn(async move {
            let mut scheduler = scheduler;
            {
                let mut shutdown_future = Box::pin(scheduler.shutdown());
                let mut started = Some(shutdown_started);
                std::future::poll_fn(|context| {
                    let result = shutdown_future.as_mut().poll(context);
                    if let Some(started) = started.take() {
                        let _ = started.send(());
                    }
                    result
                })
                .await;
            }
            scheduler
        });
        shutdown_ready.await.unwrap();
        assert_eq!(executor.active.load(Ordering::SeqCst), 1);
        assert!(
            ids.iter()
                .all(|id| f.leases.lock().unwrap().contains_key(id)),
            "claims remain owned until the in-flight callback finishes"
        );
        executor.release.add_permits(1);
        let mut scheduler = shutdown.await.unwrap();

        assert_eq!(executor.active.load(Ordering::SeqCst), 0);
        assert_eq!(
            executor.completed.load(Ordering::SeqCst),
            1,
            "the in-flight Execute callback returned instead of being aborted"
        );
        assert!(
            ids.iter()
                .all(|id| !f.leases.lock().unwrap().contains_key(id))
        );
        assert_eq!(
            executor.started.available_permits(),
            0,
            "queued work never entered Execute after stop"
        );
        assert!(f.records.lock().unwrap().is_empty());
        assert!(f.projections.lock().unwrap().is_empty());
        assert!(scheduler.dispatch(vec![f.job()]).await.is_err());
    })
    .await
    .expect("shutdown must quiesce after the controlled Execute callback is released");
}

#[tokio::test]
async fn rejected_durable_claim_never_dispatches_and_releases_local_claim() {
    let mut f = Fixture::new();
    Arc::get_mut(&mut f).unwrap().claim_fails = true;
    let job = f.job();
    let scope = job.id.clone();
    let mut scheduler = Scheduler::new(scope.clone(), SchedulerConfig::default(), f.ports());
    assert!(scheduler.dispatch(vec![job.clone()]).await.is_err());
    scheduler.drain().await.unwrap();
    assert_eq!(f.calls.load(Ordering::SeqCst), 0);
    assert!(!f.leases.lock().unwrap().contains_key(&job.id));
}

#[tokio::test]
async fn one_shot_execution_requires_a_durable_claim() {
    let mut f = Fixture::new();
    Arc::get_mut(&mut f).unwrap().claim_fails = true;
    let mut job = f.job();
    job.schedule = Schedule::At { at: f.now() };
    let mut scheduler = Scheduler::new(job.id.clone(), SchedulerConfig::default(), f.ports());
    assert!(scheduler.dispatch(vec![job]).await.is_err());
    scheduler.drain().await.unwrap();
    assert_eq!(f.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn execution_definition_includes_every_execution_and_delivery_input() {
    let f = Fixture::new();
    let job = f.job();
    let original = serde_json::to_value(&job).unwrap();
    for (field, value) in [
        ("command", serde_json::json!("changed")),
        ("prompt", serde_json::json!("changed")),
        ("job_type", serde_json::json!("agent")),
        ("session_target", serde_json::json!("main")),
        ("model", serde_json::json!("changed")),
        ("agent_id", serde_json::json!("changed")),
        (
            "delivery",
            serde_json::json!({"mode":"announce","best_effort":false}),
        ),
        (
            "origin",
            serde_json::json!({"kind":"web","thread_id":"changed"}),
        ),
        (
            "policy",
            serde_json::json!({"retries":0,"single_flight":true}),
        ),
        ("delete_after_run", serde_json::json!(true)),
    ] {
        let mut edited = original.clone();
        edited[field] = value;
        let edited: CronJob = serde_json::from_value(edited).unwrap();
        assert!(!job.same_execution_definition(&edited), "{field}");
    }
    let mut bookkeeping = job.clone();
    bookkeeping.name = Some("renamed".into());
    bookkeeping.last_output = Some("completed".into());
    assert!(job.same_execution_definition(&bookkeeping));
}

#[tokio::test]
async fn explicit_failed_delivery_prevents_auto_delete_only_when_required() {
    for best_effort in [false, true] {
        let mut f = Fixture::new();
        Arc::get_mut(&mut f).unwrap().delivery_status_failed = true;
        let mut job = f.job();
        job.schedule = Schedule::At { at: f.now() };
        job.delete_after_run = true;
        job.delivery.best_effort = best_effort;
        let mut scheduler = Scheduler::new(job.id.clone(), SchedulerConfig::default(), f.ports());
        scheduler.dispatch(vec![job]).await.unwrap();
        scheduler.drain().await.unwrap();
        let completions = f.completions.lock().unwrap();
        assert_eq!(completions[0].success, best_effort);
        assert_eq!(completions[0].remove, best_effort);
        assert_eq!(completions[0].disable, !best_effort);
        let records = f.records.lock().unwrap();
        assert_eq!(records[0].status, if best_effort { "ok" } else { "error" });
        assert_eq!(records[0].delivery_status, Some(DeliveryStatus::Failed));
        let projection = &f.projections.lock().unwrap()[0];
        assert_eq!(projection.success, best_effort);
        assert_eq!(projection.delivery_status, Some(DeliveryStatus::Failed));
    }
}

#[tokio::test]
async fn deferred_active_head_does_not_starve_later_due_jobs_in_a_limited_batch() {
    let mut fixture = Fixture::new();
    Arc::get_mut(&mut fixture).unwrap().max_tasks = 1;
    let first = fixture.job();
    let second = fixture.job();
    *fixture.jobs.lock().unwrap() = vec![first.clone(), second.clone()];
    let scope = uuid::Uuid::new_v4().to_string();
    let held = fixture
        .claim_due_occurrence(&first, "held", fixture.now(), chrono::Duration::hours(1))
        .await
        .unwrap()
        .unwrap();
    let mut scheduler = Scheduler::new(scope, SchedulerConfig::default(), fixture.ports());
    assert_eq!(scheduler.tick().await.unwrap(), 1);
    scheduler.drain().await.unwrap();
    assert_eq!(fixture.records.lock().unwrap()[0].job_id, second.id);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.jobs.lock().unwrap()[0].next_run, first.next_run);
    assert_eq!(held.job.id, first.id);
}
