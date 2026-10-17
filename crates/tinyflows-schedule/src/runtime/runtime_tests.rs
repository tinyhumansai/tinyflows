use super::*;
use crate::{DeliveryStatus, JobType, Schedule};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tinyflows_bus::schedule::DeliveryProjection;
use tinyflows_bus::schedule::{OccurrenceClaim, RunJobRefusal};
use tokio::sync::Notify;

struct Fixture {
    now: Mutex<DateTime<Utc>>,
    waits: Mutex<Vec<Duration>>,
    jobs: Mutex<Vec<CronJob>>,
    leases: Mutex<HashMap<String, (String, DateTime<Utc>)>>,
    records: Mutex<Vec<RunRecord>>,
    completions: Mutex<Vec<Completion>>,
    projections: Mutex<Vec<DeliveryProjection>>,
    raw_details: Mutex<Vec<Option<String>>>,
    calls: AtomicUsize,
    triggers: AtomicUsize,
    trigger_jobs: Mutex<Vec<CronJob>>,
    trigger_fail_count: usize,
    fail_count: usize,
    permanent: bool,
    delivery_fails: bool,
    delivery_status_failed: bool,
    claim_fails: bool,
    claim_attempts: AtomicUsize,
    claim_fails_after: Mutex<Option<usize>>,
    max_tasks: usize,
    renewals: AtomicUsize,
    renewed: Notify,
    take_lease_on_renew: AtomicBool,
    lease_takeover_job: Mutex<Option<String>>,
    lease_taken_over: Notify,
    control_retry_heartbeat: AtomicBool,
    backoff_entered: Notify,
    release_backoff: Notify,
    heartbeat_entered: Notify,
    release_heartbeat: Notify,
}
impl Fixture {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            now: Mutex::new("2026-01-01T00:00:00Z".parse().unwrap()),
            waits: Mutex::new(vec![]),
            jobs: Mutex::new(vec![]),
            leases: Mutex::new(HashMap::new()),
            records: Mutex::new(vec![]),
            completions: Mutex::new(vec![]),
            projections: Mutex::new(vec![]),
            raw_details: Mutex::new(vec![]),
            calls: AtomicUsize::new(0),
            triggers: AtomicUsize::new(0),
            trigger_jobs: Mutex::new(vec![]),
            trigger_fail_count: 0,
            fail_count: 0,
            permanent: false,
            delivery_fails: false,
            delivery_status_failed: false,
            claim_fails: false,
            claim_attempts: AtomicUsize::new(0),
            claim_fails_after: Mutex::new(None),
            max_tasks: usize::MAX,
            renewals: AtomicUsize::new(0),
            renewed: Notify::new(),
            take_lease_on_renew: AtomicBool::new(false),
            lease_takeover_job: Mutex::new(None),
            lease_taken_over: Notify::new(),
            control_retry_heartbeat: AtomicBool::new(false),
            backoff_entered: Notify::new(),
            release_backoff: Notify::new(),
            heartbeat_entered: Notify::new(),
            release_heartbeat: Notify::new(),
        })
    }
    fn ports(self: &Arc<Self>) -> RuntimePorts {
        RuntimePorts {
            store: self.clone(),
            executor: self.clone(),
            delivery: self.clone(),
            triggers: self.clone(),
            clock: self.clone(),
        }
    }
    fn job(&self) -> CronJob {
        let now = self.now();
        CronJob {
            claim_generation: 0,
            id: uuid::Uuid::new_v4().to_string(),
            expression: String::new(),
            command: "echo ok".into(),
            prompt: None,
            name: None,
            schedule: Schedule::Every { every_ms: 60_000 },
            job_type: JobType::Shell,
            session_target: Default::default(),
            model: None,
            agent_id: None,
            enabled: true,
            delivery: Default::default(),
            delete_after_run: false,
            created_at: now,
            next_run: now,
            last_run: None,
            last_status: None,
            last_output: None,
            origin: None,
            policy: Default::default(),
        }
    }
}
#[async_trait]
impl Clock for Fixture {
    fn now(&self) -> DateTime<Utc> {
        *self.now.lock().unwrap()
    }
    async fn sleep(&self, duration: Duration) {
        if self.control_retry_heartbeat.load(Ordering::SeqCst) {
            self.backoff_entered.notify_one();
            self.release_backoff.notified().await;
            return;
        }
        self.waits.lock().unwrap().push(duration);
        *self.now.lock().unwrap() += chrono::Duration::from_std(duration).unwrap();
    }
    async fn lease_wait(&self, _: Duration) {
        if self.control_retry_heartbeat.load(Ordering::SeqCst) {
            self.heartbeat_entered.notify_one();
            self.release_heartbeat.notified().await;
        } else {
            std::future::pending().await
        }
    }
}
#[async_trait]
impl JobStore for Fixture {
    async fn due_jobs(&self, _: DateTime<Utc>) -> Result<Vec<CronJob>> {
        Ok(self
            .jobs
            .lock()
            .unwrap()
            .iter()
            .take(self.max_tasks)
            .cloned()
            .collect())
    }
    async fn due_jobs_excluding(
        &self,
        _: DateTime<Utc>,
        excluded: &[String],
    ) -> Result<Vec<CronJob>> {
        Ok(self
            .jobs
            .lock()
            .unwrap()
            .iter()
            .filter(|job| !excluded.contains(&job.id))
            .take(self.max_tasks)
            .cloned()
            .collect())
    }
    async fn claim_due_occurrence(
        &self,
        job: &CronJob,
        owner: &str,
        now: DateTime<Utc>,
        lease: chrono::Duration,
    ) -> Result<Option<OccurrenceClaim>> {
        let attempt = self.claim_attempts.fetch_add(1, Ordering::SeqCst);
        anyhow::ensure!(
            !self.claim_fails && *self.claim_fails_after.lock().unwrap() != Some(attempt),
            "durable claim rejected"
        );
        let mut leases = self.leases.lock().unwrap();
        if leases
            .get(&job.id)
            .is_some_and(|(_, expires)| *expires > now)
        {
            return Ok(None);
        }
        let token = uuid::Uuid::new_v4().to_string();
        leases.insert(job.id.clone(), (token.clone(), now + lease));
        Ok(Some(OccurrenceClaim {
            receipt: token,
            job: job.clone(),
            occurrence: job.next_run,
            started_at: now,
            owner: owner.into(),
            run_id: None,
        }))
    }
    async fn claim_manual_occurrence(
        &self,
        job: &CronJob,
        owner: &str,
        now: DateTime<Utc>,
        lease: chrono::Duration,
    ) -> Result<Result<OccurrenceClaim, RunJobRefusal>> {
        match self.claim_due_occurrence(job, owner, now, lease).await? {
            Some(claim) => Ok(Ok(claim)),
            None => Ok(Err(RunJobRefusal::AlreadyRunning)),
        }
    }
    async fn renew_occurrence(
        &self,
        claim: &OccurrenceClaim,
        now: DateTime<Utc>,
        lease: chrono::Duration,
    ) -> Result<bool> {
        let mut leases = self.leases.lock().unwrap();
        let takeover_job = self.lease_takeover_job.lock().unwrap().take();
        if takeover_job.as_deref() == Some(claim.job.id.as_str())
            || self.take_lease_on_renew.swap(false, Ordering::SeqCst)
        {
            leases.insert(
                claim.job.id.clone(),
                ("successor-owner".into(), now + lease),
            );
            drop(leases);
            self.lease_taken_over.notify_one();
            return Ok(false);
        }
        if let Some(other_job) = takeover_job {
            *self.lease_takeover_job.lock().unwrap() = Some(other_job);
        }
        let Some((token, expires)) = leases.get_mut(&claim.job.id) else {
            return Ok(false);
        };
        if token != &claim.receipt || *expires <= now {
            return Ok(false);
        }
        *expires = (*expires).max(now + lease);
        self.renewals.fetch_add(1, Ordering::SeqCst);
        drop(leases);
        self.renewed.notify_one();
        Ok(true)
    }
    async fn release_occurrence(&self, claim: &OccurrenceClaim, _: DateTime<Utc>) -> Result<bool> {
        let mut leases = self.leases.lock().unwrap();
        if leases
            .get(&claim.job.id)
            .is_some_and(|(token, _)| token == &claim.receipt)
        {
            leases.remove(&claim.job.id);
            return Ok(true);
        }
        Ok(false)
    }
    async fn queue_occurrence(&self, _: &OccurrenceClaim, record: RunRecord) -> Result<bool> {
        self.records.lock().unwrap().push(record);
        Ok(true)
    }
    async fn complete_occurrence(
        &self,
        claim: &OccurrenceClaim,
        record: RunRecord,
        completion: Completion,
    ) -> Result<bool> {
        let mut leases = self.leases.lock().unwrap();
        if !leases.get(&claim.job.id).is_some_and(|(token, expires)| {
            token == &claim.receipt && *expires > record.finished_at
        }) {
            return Ok(false);
        }
        leases.remove(&claim.job.id);
        self.records.lock().unwrap().push(record);
        self.completions.lock().unwrap().push(completion);
        Ok(true)
    }
}
#[async_trait]
impl JobExecutor for Fixture {
    async fn execute(&self, _: &CronJob, _: &str) -> Result<Attempt> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let raw_detail = {
            let mut details = self.raw_details.lock().unwrap();
            if details.is_empty() {
                None
            } else {
                details.remove(0)
            }
        };
        Ok(Attempt {
            success: call >= self.fail_count,
            output: "result".into(),
            retryable: !self.permanent,
            raw_detail,
        })
    }
}
#[async_trait]
impl RunDelivery for Fixture {
    async fn send(&self, _: &CronJob, _: &str, _: &Attempt) -> Result<DeliveryStatus> {
        if self.delivery_fails {
            anyhow::bail!("delivery unavailable");
        }
        Ok(if self.delivery_status_failed {
            DeliveryStatus::Failed
        } else {
            DeliveryStatus::NotRequested
        })
    }
    async fn deliver(&self, projection: &DeliveryProjection) -> Result<()> {
        self.projections.lock().unwrap().push(projection.clone());
        Ok(())
    }
}
#[async_trait]
impl TriggerSink for Fixture {
    async fn schedule(&self, job: &CronJob, _: &str) -> Result<Attempt> {
        let call = self.triggers.fetch_add(1, Ordering::SeqCst);
        self.trigger_jobs.lock().unwrap().push(job.clone());
        Ok(Attempt {
            success: call >= self.trigger_fail_count,
            output: "triggered".into(),
            retryable: true,
            raw_detail: None,
        })
    }
}

#[tokio::test]
async fn retries_use_clock_and_zero_override_is_one_attempt() {
    let mut f = Fixture::new();
    Arc::get_mut(&mut f).unwrap().fail_count = 2;
    let job = f.job();
    let result = execute_with_retry(&f.ports(), &SchedulerConfig::default(), &job, "stable")
        .await
        .unwrap();
    assert!(result.success);
    assert_eq!(f.calls.load(Ordering::SeqCst), 3);
    assert_eq!(
        *f.waits.lock().unwrap(),
        [Duration::from_millis(200), Duration::from_millis(600)]
    );
    let mut job = f.job();
    job.policy.retries = Some(0);
    f.calls.store(0, Ordering::SeqCst);
    assert!(
        !execute_with_retry(&f.ports(), &SchedulerConfig::default(), &job, "stable")
            .await
            .unwrap()
            .success
    );
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn permanent_failure_halts_without_waiting() {
    let mut f = Fixture::new();
    let inner = Arc::get_mut(&mut f).unwrap();
    inner.fail_count = 10;
    inner.permanent = true;
    assert!(
        !execute_with_retry(&f.ports(), &SchedulerConfig::default(), &f.job(), "run")
            .await
            .unwrap()
            .success
    );
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
    assert!(f.waits.lock().unwrap().is_empty());
}

#[tokio::test]
async fn one_shots_disable_unless_successful_auto_delete() {
    for delete in [false, true] {
        let f = Fixture::new();
        let mut job = f.job();
        job.schedule = Schedule::At { at: f.now() };
        job.delete_after_run = delete;
        let mut scheduler = Scheduler::new(job.id.clone(), SchedulerConfig::default(), f.ports());
        scheduler.dispatch(vec![job]).await.unwrap();
        scheduler.drain().await.unwrap();
        let c = f.completions.lock().unwrap();
        assert_eq!(c[0].remove, delete);
        assert_eq!(c[0].disable, !delete);
        assert!(c[0].next_run.is_none());
    }
}

#[tokio::test]
async fn flow_schedule_dispatches_directly_to_trigger_sink() {
    let f = Fixture::new();
    let mut job = f.job();
    job.job_type = JobType::Flow;
    let mut scheduler = Scheduler::new(job.id.clone(), SchedulerConfig::default(), f.ports());
    scheduler.dispatch(vec![job.clone()]).await.unwrap();
    scheduler.drain().await.unwrap();
    assert_eq!(f.triggers.load(Ordering::SeqCst), 1);
    assert_eq!(f.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        serde_json::to_value(&f.trigger_jobs.lock().unwrap()[0]).unwrap(),
        serde_json::to_value(&job).unwrap(),
        "trigger callback receives the complete captured job"
    );
}

#[tokio::test]
async fn required_delivery_failure_disables_failed_one_shot_without_deleting() {
    let mut f = Fixture::new();
    Arc::get_mut(&mut f).unwrap().delivery_fails = true;
    let mut job = f.job();
    job.schedule = Schedule::At { at: f.now() };
    job.delete_after_run = true;
    job.delivery.best_effort = false;
    let mut scheduler = Scheduler::new(job.id.clone(), SchedulerConfig::default(), f.ports());
    scheduler.dispatch(vec![job]).await.unwrap();
    scheduler.drain().await.unwrap();
    let c = f.completions.lock().unwrap();
    assert!(c[0].disable);
    assert!(!c[0].remove);
    assert!(!c[0].success);
    let r = f.records.lock().unwrap();
    assert_eq!(r[0].status, "error");
    assert_eq!(r[0].delivery_status, Some(DeliveryStatus::Failed));
    let projection = &f.projections.lock().unwrap()[0];
    assert!(
        !projection.success,
        "terminal delivery sees required-send failure"
    );
    assert_eq!(projection.delivery_status, Some(DeliveryStatus::Failed));
}

#[tokio::test]
async fn durable_claim_blocks_a_second_dispatch_without_advancing_the_occurrence() {
    let f = Fixture::new();
    let mut job = f.job();
    job.policy.single_flight = true;
    let scope = job.id.clone();
    let held = f
        .claim_due_occurrence(&job, "first-owner", f.now(), chrono::Duration::hours(1))
        .await
        .unwrap()
        .unwrap();
    let mut scheduler = Scheduler::new(scope, SchedulerConfig::default(), f.ports());
    assert_eq!(scheduler.dispatch(vec![job]).await.unwrap(), 0);
    assert!(f.records.lock().unwrap().is_empty());
    assert_eq!(f.calls.load(Ordering::SeqCst), 0);
    assert_eq!(held.occurrence, f.job().next_run);
}

#[test]
fn scheduler_slots_are_scoped_and_release_on_drop() {
    let a = uuid::Uuid::new_v4().to_string();
    let slot = SchedulerSlot::acquire(&a).unwrap();
    assert!(SchedulerSlot::acquire(&a).is_none());
    drop(slot);
    assert!(SchedulerSlot::acquire(&a).is_some());
}

struct BlockExecutor {
    started: Semaphore,
    release: Semaphore,
    active: AtomicUsize,
    peak: AtomicUsize,
    completed: AtomicUsize,
}
#[async_trait]
impl JobExecutor for BlockExecutor {
    async fn execute(&self, _: &CronJob, _: &str) -> Result<Attempt> {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(active, Ordering::SeqCst);
        self.started.add_permits(1);
        self.release.acquire().await.unwrap().forget();
        self.completed.fetch_add(1, Ordering::SeqCst);
        self.active.fetch_sub(1, Ordering::SeqCst);
        Ok(Attempt {
            success: true,
            output: "ok".into(),
            retryable: false,
            raw_detail: None,
        })
    }
}

#[cfg(test)]
#[path = "runtime_concurrency_tests.rs"]
mod concurrency;

#[cfg(test)]
#[path = "runtime_lease_tests.rs"]
mod lease_tests;
