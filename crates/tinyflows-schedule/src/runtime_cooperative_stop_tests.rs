use super::*;
use crate::{CronJob, DeliveryStatus, JobType, Schedule};
use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tinyflows_bus::schedule::{
    Attempt, Completion, DeliveryProjection, OccurrenceClaim, RunRecord,
};
use tokio::sync::Notify;

pub(super) struct Fixture {
    pub(super) now: DateTime<Utc>,
    pub(super) block_on_renewal: usize,
    pub(super) renewals: AtomicUsize,
    pub(super) renewal_entered: Notify,
    pub(super) release_renewal: Notify,
    pub(super) heartbeat_tick: Notify,
    pub(super) renewal_failed: Notify,
    pub(super) terminal_commit_entered: Notify,
    pub(super) release_terminal_commit: Notify,
    pub(super) persisted_completions: AtomicUsize,
    pub(super) pause_terminal_commit: bool,
    pub(super) heartbeat_renew_error: bool,
    pub(super) execution_entered: Notify,
    pub(super) release_execution: Notify,
    pub(super) pause_execution: std::sync::atomic::AtomicBool,
    pub(super) execution_in_flight: std::sync::atomic::AtomicBool,
    pub(super) released_during_execution: std::sync::atomic::AtomicBool,
    pub(super) claim_lost: std::sync::atomic::AtomicBool,
    pub(super) fail_next_renewal: std::sync::atomic::AtomicBool,
    pub(super) released_claims: AtomicUsize,
    pub(super) executions: AtomicUsize,
    pub(super) failed_callbacks: AtomicUsize,
    pub(super) deliveries: AtomicUsize,
    pub(super) fail_execution: bool,
}

impl Fixture {
    fn job(&self) -> CronJob {
        CronJob {
            claim_generation: 0,
            id: "cooperative-stop-job".into(),
            expression: String::new(),
            command: "echo stop".into(),
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
            created_at: self.now,
            next_run: self.now,
            last_run: None,
            last_status: None,
            last_output: None,
            origin: None,
            policy: tinyflows_bus::schedule::JobPolicy {
                retries: Some(0),
                ..Default::default()
            },
        }
    }

    pub(super) fn claim(&self) -> OccurrenceClaim {
        let job = self.job();
        OccurrenceClaim {
            receipt: "cooperative-stop-claim".into(),
            occurrence: self.now,
            started_at: self.now,
            owner: "cooperative-stop-owner".into(),
            job,
            run_id: Some("cooperative-stop-run".into()),
        }
    }

    pub(super) fn ports(self: &Arc<Self>) -> RuntimePorts {
        RuntimePorts {
            store: self.clone(),
            executor: self.clone(),
            delivery: self.clone(),
            triggers: self.clone(),
            clock: self.clone(),
        }
    }
}

#[async_trait]
impl JobStore for Fixture {
    async fn due_jobs(&self, _: DateTime<Utc>) -> Result<Vec<CronJob>> {
        Ok(vec![])
    }

    async fn renew_occurrence(
        &self,
        _: &OccurrenceClaim,
        _: DateTime<Utc>,
        _: chrono::Duration,
    ) -> Result<bool> {
        if self.fail_next_renewal.swap(false, Ordering::SeqCst) {
            self.renewal_failed.notify_one();
            return Err(anyhow::anyhow!("injected heartbeat transport error"));
        }
        if self.claim_lost.load(Ordering::SeqCst) {
            self.renewal_failed.notify_one();
            return Ok(false);
        }
        if self.persisted_completions.load(Ordering::SeqCst) > 0 {
            self.renewal_failed.notify_one();
            if self.heartbeat_renew_error {
                return Err(anyhow::anyhow!("injected heartbeat transport error"));
            }
            return Ok(false);
        }
        let renewal = self.renewals.fetch_add(1, Ordering::SeqCst) + 1;
        if renewal == self.block_on_renewal {
            self.renewal_entered.notify_one();
            self.release_renewal.notified().await;
        }
        Ok(true)
    }

    async fn release_occurrence(&self, _: &OccurrenceClaim, _: DateTime<Utc>) -> Result<bool> {
        self.released_claims.fetch_add(1, Ordering::SeqCst);
        if self.execution_in_flight.load(Ordering::SeqCst) {
            self.released_during_execution.store(true, Ordering::SeqCst);
        }
        Ok(true)
    }

    async fn complete_occurrence(
        &self,
        _: &OccurrenceClaim,
        _: RunRecord,
        _: Completion,
    ) -> Result<bool> {
        self.persisted_completions.fetch_add(1, Ordering::SeqCst);
        self.terminal_commit_entered.notify_one();
        if self.pause_terminal_commit {
            self.release_terminal_commit.notified().await;
        }
        Ok(true)
    }
}

#[async_trait]
impl JobExecutor for Fixture {
    async fn execute(&self, _: &CronJob, _: &str) -> Result<Attempt> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        self.execution_in_flight.store(true, Ordering::SeqCst);
        self.execution_entered.notify_one();
        if self.pause_execution.load(Ordering::SeqCst) {
            self.release_execution.notified().await;
        }
        self.execution_in_flight.store(false, Ordering::SeqCst);
        Ok(Attempt {
            success: !self.fail_execution,
            output: "result".into(),
            retryable: false,
            raw_detail: None,
        })
    }

    async fn failed(&self, _: &CronJob, _: &str, _: &Attempt) {
        self.failed_callbacks.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl RunDelivery for Fixture {
    async fn send(&self, _: &CronJob, _: &str, _: &Attempt) -> Result<DeliveryStatus> {
        self.deliveries.fetch_add(1, Ordering::SeqCst);
        Ok(DeliveryStatus::NotRequested)
    }

    async fn deliver(&self, _: &DeliveryProjection) -> Result<()> {
        self.deliveries.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait]
impl TriggerSink for Fixture {
    async fn schedule(&self, _: &CronJob, _: &str) -> Result<Attempt> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        Ok(Attempt {
            success: !self.fail_execution,
            output: "triggered".into(),
            retryable: false,
            raw_detail: None,
        })
    }
}

#[async_trait]
impl Clock for Fixture {
    fn now(&self) -> DateTime<Utc> {
        self.now
    }

    async fn sleep(&self, _: Duration) {
        std::future::pending().await
    }

    async fn lease_wait(&self, _: Duration) {
        self.heartbeat_tick.notified().await
    }
}

fn fixture(block_on_renewal: usize, fail_execution: bool) -> Arc<Fixture> {
    Arc::new(Fixture {
        now: "2026-01-01T00:00:00Z".parse().unwrap(),
        block_on_renewal,
        renewals: AtomicUsize::new(0),
        renewal_entered: Notify::new(),
        release_renewal: Notify::new(),
        heartbeat_tick: Notify::new(),
        renewal_failed: Notify::new(),
        terminal_commit_entered: Notify::new(),
        release_terminal_commit: Notify::new(),
        persisted_completions: AtomicUsize::new(0),
        pause_terminal_commit: false,
        heartbeat_renew_error: false,
        execution_entered: Notify::new(),
        release_execution: Notify::new(),
        pause_execution: std::sync::atomic::AtomicBool::new(false),
        execution_in_flight: std::sync::atomic::AtomicBool::new(false),
        released_during_execution: std::sync::atomic::AtomicBool::new(false),
        claim_lost: std::sync::atomic::AtomicBool::new(false),
        fail_next_renewal: std::sync::atomic::AtomicBool::new(false),
        released_claims: AtomicUsize::new(0),
        executions: AtomicUsize::new(0),
        failed_callbacks: AtomicUsize::new(0),
        deliveries: AtomicUsize::new(0),
        fail_execution,
    })
}

fn terminal_heartbeat_fixture(heartbeat_renew_error: bool) -> Arc<Fixture> {
    Arc::new(Fixture {
        now: "2026-01-01T00:00:00Z".parse().unwrap(),
        block_on_renewal: usize::MAX,
        renewals: AtomicUsize::new(0),
        renewal_entered: Notify::new(),
        release_renewal: Notify::new(),
        heartbeat_tick: Notify::new(),
        renewal_failed: Notify::new(),
        terminal_commit_entered: Notify::new(),
        release_terminal_commit: Notify::new(),
        persisted_completions: AtomicUsize::new(0),
        pause_terminal_commit: true,
        heartbeat_renew_error,
        execution_entered: Notify::new(),
        release_execution: Notify::new(),
        pause_execution: std::sync::atomic::AtomicBool::new(false),
        execution_in_flight: std::sync::atomic::AtomicBool::new(false),
        released_during_execution: std::sync::atomic::AtomicBool::new(false),
        claim_lost: std::sync::atomic::AtomicBool::new(false),
        fail_next_renewal: std::sync::atomic::AtomicBool::new(false),
        released_claims: AtomicUsize::new(0),
        executions: AtomicUsize::new(0),
        failed_callbacks: AtomicUsize::new(0),
        deliveries: AtomicUsize::new(0),
        fail_execution: false,
    })
}

async fn trigger_terminal_heartbeat(fixture: &Fixture) {
    fixture.terminal_commit_entered.notified().await;
    fixture.heartbeat_tick.notify_one();
    fixture.renewal_failed.notified().await;
    fixture.release_terminal_commit.notify_one();
}

fn assert_persisted_success(fixture: &Fixture, outcome: &super::ManualRunOutcome) {
    assert!(outcome.success);
    assert_eq!(fixture.persisted_completions.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.executions.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.failed_callbacks.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cooperative_heartbeat_loss_during_terminal_reply_keeps_persisted_outcome() {
    let fixture = terminal_heartbeat_fixture(false);
    let scheduler = Scheduler::new(
        "completion-heartbeat-race",
        SchedulerConfig::default(),
        fixture.ports(),
    );
    let claim = fixture.claim();
    let worker = tokio::spawn(async move {
        scheduler
            .run_manual_claim_cooperative(
                claim,
                ManualDelivery::NotRequested,
                watch::channel(false).1,
            )
            .await
    });
    trigger_terminal_heartbeat(&fixture).await;

    let outcome = worker.await.unwrap().unwrap().unwrap();
    assert_persisted_success(&fixture, &outcome);
}

#[tokio::test]
async fn scheduled_heartbeat_loss_during_terminal_reply_keeps_persisted_outcome() {
    let fixture = terminal_heartbeat_fixture(false);
    let ports = fixture.ports();
    let config = SchedulerConfig::default();
    let claim = fixture.claim();
    let worker = tokio::spawn(async move {
        run_claim_with_renewal(
            &ports,
            &config,
            claim,
            ManualDelivery::NotRequested,
            false,
            None,
            watch::channel(false).1,
        )
        .await
    });
    trigger_terminal_heartbeat(&fixture).await;

    let outcome = worker.await.unwrap().unwrap();
    assert_persisted_success(&fixture, &outcome);
}

#[tokio::test]
async fn heartbeat_transport_error_during_terminal_reply_keeps_persisted_outcome() {
    let fixture = terminal_heartbeat_fixture(true);
    let scheduler = Scheduler::new(
        "completion-heartbeat-transport-error",
        SchedulerConfig::default(),
        fixture.ports(),
    );
    let claim = fixture.claim();
    let worker = tokio::spawn(async move {
        scheduler
            .run_manual_claim_cooperative(
                claim,
                ManualDelivery::NotRequested,
                watch::channel(false).1,
            )
            .await
    });
    trigger_terminal_heartbeat(&fixture).await;

    let outcome = worker.await.unwrap().unwrap().unwrap();
    assert_persisted_success(&fixture, &outcome);
}

#[tokio::test]
async fn heartbeat_loss_during_execute_waits_for_it_then_blocks_later_callbacks() {
    let fixture = Arc::new(Fixture {
        now: "2026-01-01T00:00:00Z".parse().unwrap(),
        block_on_renewal: usize::MAX,
        renewals: AtomicUsize::new(0),
        renewal_entered: Notify::new(),
        release_renewal: Notify::new(),
        heartbeat_tick: Notify::new(),
        renewal_failed: Notify::new(),
        terminal_commit_entered: Notify::new(),
        release_terminal_commit: Notify::new(),
        persisted_completions: AtomicUsize::new(0),
        pause_terminal_commit: false,
        heartbeat_renew_error: false,
        execution_entered: Notify::new(),
        release_execution: Notify::new(),
        pause_execution: std::sync::atomic::AtomicBool::new(true),
        execution_in_flight: std::sync::atomic::AtomicBool::new(false),
        released_during_execution: std::sync::atomic::AtomicBool::new(false),
        claim_lost: std::sync::atomic::AtomicBool::new(false),
        fail_next_renewal: std::sync::atomic::AtomicBool::new(false),
        released_claims: AtomicUsize::new(0),
        executions: AtomicUsize::new(0),
        failed_callbacks: AtomicUsize::new(0),
        deliveries: AtomicUsize::new(0),
        fail_execution: false,
    });
    let scheduler = Scheduler::new(
        "lost-claim-inflight-execute",
        SchedulerConfig::default(),
        fixture.ports(),
    );
    let claim = fixture.claim();
    let worker = tokio::spawn(async move {
        scheduler
            .run_manual_claim_cooperative(claim, ManualDelivery::Requested, watch::channel(false).1)
            .await
    });
    fixture.execution_entered.notified().await;
    fixture.claim_lost.store(true, Ordering::SeqCst);
    fixture.heartbeat_tick.notify_one();
    fixture.renewal_failed.notified().await;
    fixture.release_execution.notify_one();

    assert!(worker.await.unwrap().is_err());
    assert_eq!(fixture.executions.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.failed_callbacks.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.deliveries.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.persisted_completions.load(Ordering::SeqCst), 0);
}

async fn stop_during_renewal(block_on_renewal: usize, fail_execution: bool) -> Arc<Fixture> {
    let fixture = fixture(block_on_renewal, fail_execution);
    let scheduler = Scheduler::new(
        "cooperative-stop",
        SchedulerConfig::default(),
        fixture.ports(),
    );
    let claim = fixture.claim();
    let (stop, receiver) = watch::channel(false);
    let worker = tokio::spawn(async move {
        scheduler
            .run_manual_claim_cooperative(claim, ManualDelivery::NotRequested, receiver)
            .await
    });
    fixture.renewal_entered.notified().await;
    stop.send_replace(true);
    fixture.release_renewal.notify_one();
    assert!(worker.await.unwrap().unwrap().is_none());
    fixture
}

#[tokio::test]
async fn stop_during_initial_claim_renewal_prevents_execution_callback() {
    let fixture = stop_during_renewal(1, false).await;
    assert_eq!(fixture.executions.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.failed_callbacks.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.deliveries.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn stop_during_failure_renewal_prevents_failure_callback() {
    let fixture = stop_during_renewal(2, true).await;
    assert_eq!(fixture.executions.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.failed_callbacks.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.deliveries.load(Ordering::SeqCst), 0);
}
