//! Runtime seams: the scheduler owns timing and bookkeeping; hosts own I/O.
use crate::{CronJob, DeliveryStatus};
use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::time::Duration;

pub use tinyflows_bus::schedule::{Attempt, Completion, RunRecord};
use tinyflows_bus::schedule::{DeliveryProjection, OccurrenceClaim, RunJobRefusal};

/// Storage bound to one agent/tenant scope.
#[async_trait]
pub trait JobStore: Send + Sync {
    /// Validate capabilities needed before starting a manual claim operation.
    /// The module uses this before it must retain ambiguous claim ownership.
    async fn validate_manual_claim_support(&self) -> Result<()> {
        Ok(())
    }
    /// Read enabled jobs due at `now` within the store's configured batch limit.
    async fn due_jobs(&self, now: DateTime<Utc>) -> Result<Vec<CronJob>>;
    /// Read due jobs after excluding examined identities, before applying the
    /// configured batch limit. Bounded stores must override this operation;
    /// its default fails closed instead of silently starving later jobs.
    async fn due_jobs_excluding(
        &self,
        now: DateTime<Utc>,
        excluded: &[String],
    ) -> Result<Vec<CronJob>> {
        anyhow::ensure!(
            excluded.is_empty(),
            "job store does not support due-job exclusions"
        );
        self.due_jobs(now).await
    }
    /// Atomically acquire the persisted due occurrence. Unsupported backends
    /// fail closed; a host process mutex is never an admission substitute.
    async fn claim_due_occurrence(
        &self,
        _job: &CronJob,
        _owner: &str,
        _now: DateTime<Utc>,
        _lease: chrono::Duration,
    ) -> Result<Option<OccurrenceClaim>> {
        anyhow::bail!("job store does not support durable occurrence leases")
    }
    /// Force-claim the exact saved definition for Run Now, regardless of its
    /// enabled/future schedule state.
    async fn claim_manual_occurrence(
        &self,
        _job: &CronJob,
        _owner: &str,
        _now: DateTime<Utc>,
        _lease: chrono::Duration,
    ) -> Result<Result<OccurrenceClaim, RunJobRefusal>> {
        anyhow::bail!("job store does not support durable manual admission")
    }
    /// Read back a live manual claim by the unique owner selected before
    /// admission, so a lost claim response can be reconciled without guessing.
    async fn find_owned_manual_claim(
        &self,
        _job: &CronJob,
        _owner: &str,
        _now: DateTime<Utc>,
    ) -> Result<Option<OccurrenceClaim>> {
        anyhow::bail!("job store does not support manual claim reconciliation")
    }
    /// Extend a live lease; false means this worker has lost its fence.
    async fn renew_occurrence(
        &self,
        _claim: &OccurrenceClaim,
        _now: DateTime<Utc>,
        _lease: chrono::Duration,
    ) -> Result<bool> {
        anyhow::bail!("job store does not support durable occurrence renewal")
    }
    /// Release an owned occurrence on cancellation without advancing it.
    async fn release_occurrence(
        &self,
        _claim: &OccurrenceClaim,
        _now: DateTime<Utc>,
    ) -> Result<bool> {
        anyhow::bail!("job store does not support durable occurrence release")
    }
    /// Persist the accepted manual run before returning its acknowledgement.
    async fn queue_occurrence(&self, _claim: &OccurrenceClaim, _record: RunRecord) -> Result<bool> {
        anyhow::bail!("job store does not support durable run queueing")
    }
    /// Atomically append terminal history and advance the claimed occurrence.
    async fn complete_occurrence(
        &self,
        _claim: &OccurrenceClaim,
        _record: RunRecord,
        _completion: Completion,
    ) -> Result<bool> {
        anyhow::bail!("job store does not support fenced occurrence completion")
    }
}

/// Agent and shell execution, including host security and failure classification.
#[async_trait]
pub trait JobExecutor: Send + Sync {
    /// Execute one attempt, enforcing the host's policy under the bound scope.
    async fn execute(&self, job: &CronJob, run_id: &str) -> Result<Attempt>;
    /// Product reporting after a final failed attempt; never on a retry.
    async fn failed(&self, _job: &CronJob, _run_id: &str, _attempt: &Attempt) {}
}

/// Product delivery to channels, conversation origins and alerts.
#[async_trait]
pub trait RunDelivery: Send + Sync {
    /// Perform the one actual product send; an error may fail required delivery.
    async fn send(&self, job: &CronJob, run_id: &str, attempt: &Attempt) -> Result<DeliveryStatus>;
    /// Project the module's truthful terminal decision. This callback must not
    /// send again; it receives the result only after `send` has returned.
    async fn deliver(&self, projection: &DeliveryProjection) -> Result<()>;
}

/// Direct schedule-trigger dispatch; no host domain-event round trip.
#[async_trait]
pub trait TriggerSink: Send + Sync {
    /// Fire the bound flow's schedule trigger once for this attempt.
    async fn schedule(&self, job: &CronJob, run_id: &str) -> Result<Attempt>;
}

/// Injectable wall clock and wait primitive. Tests advance time without sleeping.
#[async_trait]
pub trait Clock: Send + Sync {
    /// Current wall-clock instant used for due selection, jitter and recurrence.
    fn now(&self) -> DateTime<Utc>;
    /// Wait for a poll interval or retry delay; test clocks may advance directly.
    async fn sleep(&self, duration: Duration);
    /// Wait before renewing an occurrence lease. Tests may control this
    /// independently from retry backoff sleeps.
    async fn lease_wait(&self, duration: Duration) {
        self.sleep(duration).await;
    }
}

/// Production wall clock.
pub struct SystemClock;
#[async_trait]
impl Clock for SystemClock {
    /// Current wall-clock instant used for due selection, jitter and recurrence.
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
    /// Wait for a poll interval or retry delay; test clocks may advance directly.
    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
    /// Production lease heartbeat timer.
    async fn lease_wait(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
}
