//! Bounded background dispatch with scoped claims, retry and durable completion.
mod claims;
mod execution;
mod ports;
use crate::CronJob;
use anyhow::Result;
pub use claims::SchedulerSlot;
pub use execution::execute_with_retry;
pub use ports::*;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tinyflows_bus::schedule::{ManualDelivery, OccurrenceClaim};
use tokio::{
    sync::{Semaphore, watch},
    task::JoinSet,
};

const OCCURRENCE_LEASE: chrono::Duration = chrono::Duration::hours(2);

/// Scheduler defaults compatible with the former host loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SchedulerConfig {
    /// Poll cadence; the owning loop enforces a five-second minimum.
    pub poll_secs: u64,
    /// Jobs executing at once, clamped to at least one.
    pub max_concurrent: usize,
    /// Retries after failure unless the job supplies its own budget.
    pub retries: u32,
    /// Initial retry backoff, clamped to 200 ms and doubled up to 30 seconds.
    pub backoff_ms: u64,
}
impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            poll_secs: 30,
            max_concurrent: 4,
            retries: 2,
            backoff_ms: 200,
        }
    }
}

/// All ports belong to one scope; hosts must bind callbacks to that identity.
#[derive(Clone)]
pub struct RuntimePorts {
    /// Storage bound to this scheduler's agent/tenant.
    pub store: Arc<dyn JobStore>,
    /// Host execution and final-failure reporting.
    pub executor: Arc<dyn JobExecutor>,
    /// Product delivery, called once after execution finishes.
    pub delivery: Arc<dyn RunDelivery>,
    /// Direct flow schedule-trigger dispatch.
    pub triggers: Arc<dyn TriggerSink>,
    /// Time and waits used by both the poll and retry loops.
    pub clock: Arc<dyn Clock>,
}

/// Owns dispatched tasks. Dropping it aborts queued and executing jobs.
pub struct Scheduler {
    scope: String,
    stopped: bool,
    config: SchedulerConfig,
    ports: RuntimePorts,
    tasks: JoinSet<Result<()>>,
    permits: Arc<Semaphore>,
    owner: String,
    slot: Option<SchedulerSlot>,
    cancellation: watch::Sender<bool>,
}
impl Scheduler {
    /// Create an idle scheduler. Equal scope keys share process-wide claims.
    pub fn new(scope: impl Into<String>, config: SchedulerConfig, ports: RuntimePorts) -> Self {
        let (cancellation, _) = watch::channel(false);
        Self {
            stopped: false,
            scope: scope.into(),
            permits: Arc::new(Semaphore::new(config.max_concurrent.max(1))),
            config,
            ports,
            tasks: JoinSet::new(),
            owner: uuid::Uuid::new_v4().to_string(),
            slot: None,
            cancellation,
        }
    }

    /// Reserve the one scheduler slot shared by manual-tick and loop schedulers.
    pub fn reserve_slot(&mut self) -> bool {
        if self.slot.is_some() {
            return true;
        }
        self.slot = SchedulerSlot::acquire(&self.scope);
        self.slot.is_some()
    }

    /// Poll and dispatch immediately; a long run never blocks the next poll.
    pub async fn tick(&mut self) -> Result<usize> {
        anyhow::ensure!(!self.stopped, "scheduler is stopped");
        self.reap();
        let now = self.ports.clock.now();
        let mut batch = self.ports.store.due_jobs(now).await?;
        let limit = batch.len();
        if limit == 0 {
            return Ok(0);
        }
        let mut examined = Vec::new();
        let mut selected = Vec::new();
        loop {
            for job in batch {
                if selected.len() >= limit {
                    break;
                }
                examined.push(job.id.clone());
                let claim = self
                    .ports
                    .store
                    .claim_due_occurrence(&job, &self.owner, now, OCCURRENCE_LEASE)
                    .await;
                let claim = match claim {
                    Ok(claim) => claim,
                    Err(error) => {
                        self.release_claims(&selected).await;
                        return Err(error);
                    }
                };
                if let Some(claim) = claim {
                    selected.push(claim);
                }
            }
            if selected.len() >= limit {
                break;
            }
            batch = match self.ports.store.due_jobs_excluding(now, &examined).await {
                Ok(batch) => batch,
                Err(error) => {
                    self.release_claims(&selected).await;
                    return Err(error);
                }
            };
            if batch.is_empty() {
                break;
            }
            if batch.iter().any(|job| examined.contains(&job.id)) {
                self.release_claims(&selected).await;
                anyhow::bail!("job store returned an excluded due job");
            }
        }
        self.dispatch_claims(selected, ManualDelivery::Requested)
            .await
    }

    /// Durably claim due occurrences and return the dispatched count. A live
    /// lease is excluded without advancing or recording a skipped occurrence;
    /// expired leases remain due and can be recovered by another scheduler.
    pub async fn dispatch(&mut self, jobs: Vec<CronJob>) -> Result<usize> {
        anyhow::ensure!(!self.stopped, "scheduler is stopped");
        let mut claims = Vec::new();
        for job in jobs {
            let claimed = self
                .ports
                .store
                .claim_due_occurrence(&job, &self.owner, self.ports.clock.now(), OCCURRENCE_LEASE)
                .await;
            let claimed = match claimed {
                Ok(claimed) => claimed,
                Err(error) => {
                    self.release_claims(&claims).await;
                    return Err(error);
                }
            };
            if let Some(claim) = claimed {
                claims.push(claim);
            }
        }
        self.dispatch_claims(claims, ManualDelivery::Requested)
            .await
    }

    async fn release_claims(&self, claims: &[OccurrenceClaim]) {
        let now = self.ports.clock.now();
        for claim in claims {
            if let Err(error) = self.ports.store.release_occurrence(claim, now).await {
                tracing::warn!(%error, "failed to release an undispatched occurrence claim");
            }
        }
    }

    async fn dispatch_claims(
        &mut self,
        claims: Vec<OccurrenceClaim>,
        delivery: ManualDelivery,
    ) -> Result<usize> {
        let mut count = 0;
        for claim in claims {
            let ports = self.ports.clone();
            let config = self.config.clone();
            let cancellation = self.cancellation.subscribe();
            let permits = Arc::clone(&self.permits);
            self.tasks.spawn(async move {
                run_claim_with_renewal(
                    &ports,
                    &config,
                    claim,
                    delivery,
                    true,
                    Some(permits),
                    cancellation,
                )
                .await
                .map(|_| ())
            });
            count += 1;
        }
        Ok(count)
    }

    /// Execute a previously admitted manual run without occupying a poll-loop permit.
    pub async fn run_manual_claim(
        &self,
        claim: OccurrenceClaim,
        delivery: ManualDelivery,
    ) -> Result<ManualRunOutcome> {
        run_claim_with_renewal(
            &self.ports,
            &self.config,
            claim,
            delivery,
            false,
            None,
            self.cancellation.subscribe(),
        )
        .await
    }

    /// Execute an admitted manual run with cooperative stop at callback
    /// boundaries. `None` means stop was observed after the current callback;
    /// the caller still owns releasing the occurrence claim.
    pub async fn run_manual_claim_cooperative(
        &self,
        claim: OccurrenceClaim,
        delivery: ManualDelivery,
        mut stop: watch::Receiver<bool>,
    ) -> Result<Option<ManualRunOutcome>> {
        let work = execution::execute_and_persist_cooperative(
            &self.ports,
            &self.config,
            &claim,
            delivery,
            &mut stop,
        );
        tokio::pin!(work);
        loop {
            tokio::select! {
                _ = self.ports.clock.lease_wait(Duration::from_secs(30)) => {
                    match self.ports.store.renew_occurrence(
                        &claim,
                        self.ports.clock.now(),
                        OCCURRENCE_LEASE,
                    ).await {
                        Ok(true) => {}
                        // The renewal may race a terminal commit, whose job
                        // fence disappears before its callback reply arrives.
                        // Keep the in-flight operation alive; its own claim
                        // checks still fence any later callbacks.
                        Ok(false) | Err(_) => return work.as_mut().await,
                    }
                }
                result = &mut work => return result,
            }
        }
    }

    /// Wait for every dispatched job, returning persistence or task failures.
    pub async fn drain(&mut self) -> Result<()> {
        let mut first_error = None;
        while let Some(result) = self.tasks.join_next().await {
            if let Err(error) = result.map_err(anyhow::Error::from).and_then(|r| r) {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    /// Cancel all running/queued tasks and release their claims before returning.
    pub async fn shutdown(&mut self) {
        self.stopped = true;
        let _ = self.cancellation.send(true);
        while self.tasks.join_next().await.is_some() {}
        self.slot.take();
    }

    fn reap(&mut self) {
        while let Some(result) = self.tasks.try_join_next() {
            if let Err(error) = result.map_err(anyhow::Error::from).and_then(|r| r) {
                tracing::warn!(%error, "scheduled run failed");
            }
        }
    }

    /// One loop per scope; a second loop exits without polling.
    pub async fn run(&mut self) -> Result<()> {
        if !self.reserve_slot() {
            return Ok(());
        }
        loop {
            if let Err(error) = self.tick().await {
                tracing::warn!(%error, "scheduler poll failed");
            }
            self.ports
                .clock
                .sleep(Duration::from_secs(self.config.poll_secs.max(5)))
                .await;
        }
    }
}

/// Terminal result returned by a blocking manual run.
#[derive(Debug, Clone)]
pub struct ManualRunOutcome {
    /// Final host-classified attempt.
    pub attempt: Attempt,
    /// Final execution-plus-delivery decision.
    pub success: bool,
    /// Actual send result, absent when delivery was intentionally skipped.
    pub delivery_status: Option<crate::DeliveryStatus>,
}

async fn run_claim_with_renewal(
    ports: &RuntimePorts,
    config: &SchedulerConfig,
    claim: OccurrenceClaim,
    delivery: ManualDelivery,
    advance_schedule: bool,
    permits: Option<Arc<Semaphore>>,
    mut cancellation: watch::Receiver<bool>,
) -> Result<ManualRunOutcome> {
    let work_cancellation = cancellation.clone();
    let (work_stop, mut work_stop_receiver) = watch::channel(false);
    let work = async {
        let _permit = match permits {
            Some(permits) => Some(permits.acquire_owned().await?),
            None => None,
        };
        anyhow::ensure!(
            ports
                .store
                .renew_occurrence(&claim, ports.clock.now(), OCCURRENCE_LEASE)
                .await?,
            "scheduler lost the occurrence before execution"
        );
        anyhow::ensure!(
            !*work_cancellation.borrow(),
            "scheduler stopped before execution"
        );
        execution::execute_and_persist_inner(
            ports,
            config,
            &claim,
            delivery,
            advance_schedule,
            Some(&mut work_stop_receiver),
        )
        .await?
        .ok_or_else(|| anyhow::anyhow!("scheduler stopped before occurrence completion"))
    };
    tokio::pin!(work);
    let mut renewals_enabled = true;
    loop {
        tokio::select! {
            biased;
            changed = cancellation.changed() => {
                if changed.is_ok() && *cancellation.borrow() {
                    work_stop.send_replace(true);
                }
            }
            _ = ports.clock.lease_wait(Duration::from_secs(30)), if renewals_enabled => {
                match ports.store.renew_occurrence(&claim, ports.clock.now(), OCCURRENCE_LEASE).await {
                    Ok(true) => {}
                    // A terminal commit may already have cleared the fence
                    // while its response is still in flight. Await the work;
                    // its callback boundaries remain guarded by claim checks.
                    Ok(false) | Err(_) => {
                        renewals_enabled = false;
                        work_stop.send_replace(true);
                    }
                }
            }
            result = &mut work => {
                return release_claim_after_work_error(ports, &claim, result).await;
            }
        }
    }
}

async fn release_claim_after_work_error(
    ports: &RuntimePorts,
    claim: &OccurrenceClaim,
    result: Result<ManualRunOutcome>,
) -> Result<ManualRunOutcome> {
    if result.is_err() {
        let _ = ports
            .store
            .release_occurrence(claim, ports.clock.now())
            .await;
    }
    result
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "../runtime_cooperative_stop_tests.rs"]
mod cooperative_stop_tests;

#[cfg(test)]
#[path = "../runtime_heartbeat_cleanup_tests.rs"]
mod heartbeat_cleanup_tests;
