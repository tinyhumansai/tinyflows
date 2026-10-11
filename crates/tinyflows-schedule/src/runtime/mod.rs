//! Bounded background dispatch with scoped claims, retry and durable completion.
mod claims;
mod execution;
mod ports;
use crate::{CronJob, Schedule, next_run_for_schedule};
use anyhow::Result;
pub use claims::{RunClaim, SchedulerSlot};
pub use execution::execute_with_retry;
pub use ports::*;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio::{sync::Semaphore, task::JoinSet};

/// Stored output for a single-flight recurrence missed during an active run.
pub const SKIPPED_OUTPUT: &str = "skipped: the previous run was still in flight";

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
}
impl Scheduler {
    /// Create an idle scheduler. Equal scope keys share process-wide claims.
    pub fn new(scope: impl Into<String>, config: SchedulerConfig, ports: RuntimePorts) -> Self {
        Self {
            stopped: false,
            scope: scope.into(),
            permits: Arc::new(Semaphore::new(config.max_concurrent.max(1))),
            config,
            ports,
            tasks: JoinSet::new(),
        }
    }

    /// Poll and dispatch immediately; a long run never blocks the next poll.
    pub async fn tick(&mut self) -> Result<usize> {
        anyhow::ensure!(!self.stopped, "scheduler is stopped");
        self.reap();
        let jobs = self.ports.store.due_jobs(self.ports.clock.now()).await?;
        let limit = jobs.len();
        let now = self.ports.clock.now();
        let mut examined = Vec::new();
        let mut selected = Vec::new();
        let mut batch = jobs;
        loop {
            let mut deferred = false;
            for job in batch {
                examined.push(job.id.clone());
                if RunClaim::is_running(&self.scope, &job.id)
                    && (!job.policy.single_flight || matches!(job.schedule, Schedule::At { .. }))
                {
                    deferred = true;
                } else if selected.len() < limit {
                    selected.push(job);
                }
            }
            if !deferred || selected.len() == limit {
                break;
            }
            batch = self.ports.store.due_jobs_excluding(now, &examined).await?;
            if batch.is_empty() {
                break;
            }
            anyhow::ensure!(
                batch.iter().all(|job| !examined.contains(&job.id)),
                "job store returned an excluded due job"
            );
        }
        self.dispatch(selected).await
    }

    /// Durably claim jobs, advance recurring slots, and return the queued count.
    /// A claimed single-flight recurrence records a skipped run; one-shots wait.
    /// One-shots are disabled before dispatch: a crash after claiming can lose
    /// execution or history, but persistence failure cannot automatically replay
    /// an external side effect. Operators may explicitly re-enable such a job.
    pub async fn dispatch(&mut self, jobs: Vec<CronJob>) -> Result<usize> {
        anyhow::ensure!(!self.stopped, "scheduler is stopped");
        let mut count = 0;
        for job in jobs {
            let Some(claim) = RunClaim::acquire(&self.scope, &job.id) else {
                self.skip_in_flight(&job).await?;
                continue;
            };
            self.claim_slot(&job).await?;
            let permits = Arc::clone(&self.permits);
            let ports = self.ports.clone();
            let config = self.config.clone();
            self.tasks.spawn(async move {
                let _claim = claim;
                let _permit = permits.acquire_owned().await?;
                execution::execute_and_persist(&ports, &config, &job).await
            });
            count += 1;
        }
        Ok(count)
    }

    async fn claim_slot(&self, job: &CronJob) -> Result<()> {
        let next = if matches!(job.schedule, Schedule::At { .. }) {
            job.next_run
        } else {
            next_run_for_schedule(&job.schedule, self.ports.clock.now())?
        };
        self.ports.store.claim_slot(job, next).await
    }

    async fn skip_in_flight(&self, job: &CronJob) -> Result<()> {
        if !job.policy.single_flight || matches!(job.schedule, Schedule::At { .. }) {
            return Ok(());
        }
        self.claim_slot(job).await?;
        let now = self.ports.clock.now();
        self.ports
            .store
            .record_run(RunRecord {
                run_id: uuid::Uuid::new_v4().to_string(),
                job_id: job.id.clone(),
                job_created_at: job.created_at,
                started_at: now,
                finished_at: now,
                status: "skipped".into(),
                output: SKIPPED_OUTPUT.into(),
                delivery_status: None,
            })
            .await?;
        Ok(())
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
        self.tasks.abort_all();
        while self.tasks.join_next().await.is_some() {}
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
        let Some(_slot) = SchedulerSlot::acquire(&self.scope) else {
            return Ok(());
        };
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

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
