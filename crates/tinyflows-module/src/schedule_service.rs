//! Separate Schedule object in the same loadable TinyFlows module.
#[path = "schedule_clock.rs"]
mod clock;
#[path = "schedule_manual_workers.rs"]
mod manual_workers;
use super::schedule_callbacks::HostCallbacks;
use serde_json::Value;
use std::{collections::HashMap, sync::Arc};
use tinybus::{Connection, Error, Result};
use tinyflows_bus::schedule::{
    self, AcknowledgeManualRun, CheckOriginRequest, CheckPatchRequest, ManualRunHandle,
    ManualRunState, NextRunRequest, PrepareManualRun, RunJobNow, RunJobReply,
    RunsCloserThanRequest, StartManualRun, StartScheduler, StopManualRun, TooFrequent,
    ValidateScheduleRequest, WaitManualRun,
};
use tinyflows_schedule::{
    DeliveryConfig, JobOrigin, Schedule, runtime::*, schedule as maths, store, validation,
};
use tokio::{
    sync::{Mutex, oneshot},
    task::JoinHandle,
};

enum Running {
    Manual(Arc<Mutex<Scheduler>>),
    Loop {
        task: JoinHandle<()>,
        shutdown: Option<oneshot::Sender<()>>,
    },
}

pub(super) struct ScheduleService {
    connection: Connection,
    runtimes: Mutex<HashMap<String, Running>>,
    manual_workers: Mutex<manual_workers::ManualWorkers>,
}
impl Drop for ScheduleService {
    fn drop(&mut self) {
        for running in self.runtimes.get_mut().values() {
            if let Running::Loop { task, .. } = running {
                task.abort();
            }
        }
        self.manual_workers.get_mut().request_stop_all();
    }
}
impl ScheduleService {
    pub(super) fn new(connection: Connection) -> Self {
        Self {
            connection,
            runtimes: Mutex::new(HashMap::new()),
            manual_workers: Mutex::new(manual_workers::ManualWorkers::default()),
        }
    }
    async fn manual(&self, handle: &str) -> Result<Arc<Mutex<Scheduler>>> {
        match self.runtimes.lock().await.get(handle) {
            Some(Running::Manual(runtime)) => Ok(runtime.clone()),
            Some(_) => Err(Error::failed("scheduler owns its poll loop")),
            None => Err(Error::failed("unknown scheduler handle")),
        }
    }
}
fn failure(error: impl std::fmt::Display) -> Error {
    Error::failed(error.to_string())
}

async fn persist_manual_queue(
    store: &dyn JobStore,
    claim: &tinyflows_bus::schedule::OccurrenceClaim,
    record: RunRecord,
    now: chrono::DateTime<chrono::Utc>,
) -> anyhow::Result<bool> {
    match store.queue_occurrence(claim, record).await {
        Ok(true) => Ok(true),
        Ok(false) => {
            let _ = store.release_occurrence(claim, now).await;
            Ok(false)
        }
        Err(error) => {
            if let Err(release_error) = store.release_occurrence(claim, now).await {
                tracing::warn!(%release_error, "manual admission failed to release its queue claim");
            }
            Err(error)
        }
    }
}

#[tinybus::interface(name = "ai.tinyhumans.tinyflows.Schedule")]
impl ScheduleService {
    async fn schedule_store(
        &self,
        request: tinyflows_bus::schedule::StoreRequest,
    ) -> Result<Value> {
        super::schedule_store::Store::request(self.connection.clone(), request)
            .await
            .map_err(failure)
    }

    async fn start_scheduler(&self, request: StartScheduler) -> Result<String> {
        let now: Option<chrono::DateTime<chrono::Utc>> = request
            .config
            .get("now")
            .map(|now| serde_json::from_value(now.clone()))
            .transpose()
            .map_err(failure)?;
        if now.is_some() && !request.manual_ticks {
            return Err(Error::failed("a fixed calendar time requires manual ticks"));
        }
        let config: SchedulerConfig = serde_json::from_value(request.config).map_err(failure)?;
        let mut ports = HostCallbacks::ports(self.connection.clone(), request.callback);
        if let Some(now) = now {
            ports.clock = Arc::new(clock::FixedClock(now));
        }
        if let Some(storage) = request.storage {
            ports.store = Arc::new(
                super::schedule_store::Store::new(self.connection.clone(), storage)
                    .await
                    .map_err(failure)?,
            );
        }
        let mut scheduler = Scheduler::new(request.scope, config, ports);
        if !scheduler.reserve_slot() {
            return Err(Error::failed("a scheduler already owns this scope"));
        }
        let handle = uuid::Uuid::new_v4().to_string();
        let running = if request.manual_ticks {
            Running::Manual(Arc::new(Mutex::new(scheduler)))
        } else {
            let (shutdown, stopped) = oneshot::channel();
            let task = tokio::spawn(async move {
                tokio::select! {
                    result = scheduler.run() => if let Err(error) = result { tracing::warn!(%error, "schedule loop stopped"); },
                    _ = stopped => {},
                }
                scheduler.shutdown().await;
            });
            Running::Loop {
                task,
                shutdown: Some(shutdown),
            }
        };
        self.runtimes.lock().await.insert(handle.clone(), running);
        Ok(handle)
    }

    async fn stop_scheduler(&self, handle: String) -> Result<()> {
        let running = self
            .runtimes
            .lock()
            .await
            .remove(&handle)
            .ok_or_else(|| Error::failed("unknown scheduler handle"))?;
        // Shut down and join the owning loop, then release every queued and
        // running job claim before a replacement scheduler can poll.
        match running {
            Running::Loop {
                mut task,
                mut shutdown,
            } => {
                if let Some(shutdown) = shutdown.take() {
                    let _ = shutdown.send(());
                }
                (&mut task).await.map_err(failure)?;
            }
            Running::Manual(runtime) => runtime.lock().await.shutdown().await,
        }
        Ok(())
    }

    async fn tick_scheduler(&self, handle: String) -> Result<usize> {
        self.manual(&handle)
            .await?
            .lock()
            .await
            .tick()
            .await
            .map_err(failure)
    }
    async fn drain_scheduler(&self, handle: String) -> Result<()> {
        self.manual(&handle)
            .await?
            .lock()
            .await
            .drain()
            .await
            .map_err(failure)
    }

    async fn run_job_now(&self, request: RunJobNow) -> Result<RunJobReply> {
        let config: SchedulerConfig = serde_json::from_value(request.config).map_err(failure)?;
        let ports = HostCallbacks::ports(self.connection.clone(), request.callback);
        let mut ports = ports;
        ports.store = Arc::new(
            super::schedule_store::Store::new(self.connection.clone(), request.storage)
                .await
                .map_err(failure)?,
        );
        let now = ports.clock.now();
        let owner = uuid::Uuid::new_v4().to_string();
        let claim = match ports
            .store
            .claim_manual_occurrence(&request.job, &owner, now, chrono::Duration::hours(2))
            .await
            .map_err(failure)?
        {
            Ok(claim) => claim,
            Err(reason) => return Ok(RunJobReply::Refused { reason }),
        };
        let mut claim = claim;
        let run_id = if request.run_id.trim().is_empty() {
            uuid::Uuid::new_v4().to_string()
        } else {
            request.run_id
        };
        claim.run_id = Some(run_id.clone());
        let queued_at = ports.clock.now();
        let queued = tinyflows_schedule::runtime::RunRecord {
            run_id: run_id.clone(),
            job_id: claim.job.id.clone(),
            job_created_at: claim.job.created_at,
            started_at: claim.started_at,
            finished_at: queued_at,
            status: "queued".into(),
            output: String::new(),
            delivery_status: None,
        };
        if !persist_manual_queue(ports.store.as_ref(), &claim, queued, queued_at)
            .await
            .map_err(failure)?
        {
            return Ok(RunJobReply::Refused {
                reason: tinyflows_bus::schedule::RunJobRefusal::DefinitionChanged,
            });
        }
        let scheduler = Scheduler::new(request.scope, config, ports);
        if request.wait {
            let outcome = scheduler
                .run_manual_claim(claim, request.delivery)
                .await
                .map_err(failure)?;
            Ok(RunJobReply::Completed {
                run_id,
                attempt: outcome.attempt,
                success: outcome.success,
                delivery_status: outcome.delivery_status,
            })
        } else {
            tokio::spawn(async move {
                if let Err(error) = scheduler.run_manual_claim(claim, request.delivery).await {
                    tracing::warn!(%error, "manual schedule run did not reach terminal persistence");
                }
            });
            Ok(RunJobReply::Accepted { run_id })
        }
    }

    async fn prepare_manual_run(&self, request: PrepareManualRun) -> Result<ManualRunHandle> {
        Ok(self.manual_workers.lock().await.prepare(request))
    }

    async fn start_manual_run(&self, request: StartManualRun) -> Result<ManualRunState> {
        let mut workers = self.manual_workers.lock().await;
        let reservation = workers
            .get(&request.handle)
            .ok_or_else(|| Error::failed("unknown, expired, or acknowledged manual run handle"))?;
        let fingerprint = serde_json::to_value(&request.request).map_err(failure)?;
        let should_spawn = reservation
            .entry
            .begin_start(fingerprint)
            .map_err(failure)?;
        let state = reservation.entry.state();
        if should_spawn {
            let connection = self.connection.clone();
            let entry = reservation.entry.clone();
            let scope = reservation.scope.clone();
            let run_request = request.request;
            tokio::spawn(async move {
                manual_workers::run_manual_worker(connection, scope, run_request, entry).await;
            });
        }
        // The entry is already atomically Starting before the worker task is
        // scheduled. Duplicate identical starts only observe this state.
        let _ = &reservation.request_id;
        drop(workers);
        Ok(state)
    }

    async fn wait_manual_run(&self, request: WaitManualRun) -> Result<Option<ManualRunState>> {
        Ok(self
            .manual_workers
            .lock()
            .await
            .get(&request.handle)
            .map(|reservation| reservation.entry.state()))
    }

    async fn stop_manual_run(&self, request: StopManualRun) -> Result<Option<ManualRunState>> {
        Ok(self
            .manual_workers
            .lock()
            .await
            .get(&request.handle)
            .map(|reservation| reservation.entry.request_stop()))
    }

    async fn acknowledge_manual_run(&self, request: AcknowledgeManualRun) -> Result<bool> {
        Ok(self
            .manual_workers
            .lock()
            .await
            .acknowledge(&request.handle))
    }

    async fn parse_human_delay(&self, delay: String) -> Result<i64> {
        validation::parse_human_delay(&delay)
            .map(|d| d.num_milliseconds())
            .map_err(failure)
    }
    async fn validate_delivery(
        &self,
        delivery: Value,
        origin: Option<Value>,
        allowed_users: Option<Vec<String>>,
    ) -> Result<()> {
        let delivery: DeliveryConfig = serde_json::from_value(delivery).map_err(failure)?;
        let origin: Option<JobOrigin> = origin
            .map(serde_json::from_value)
            .transpose()
            .map_err(failure)?;
        validation::validate_delivery(&delivery, origin.as_ref(), allowed_users.as_deref())
            .map_err(failure)
    }

    async fn next_run(&self, request: NextRunRequest) -> Result<chrono::DateTime<chrono::Utc>> {
        // The contract promises an occurrence strictly after `from`; the
        // in-process function returns a one-shot's instant unchanged, so an
        // elapsed `At` is refused here instead of handed back as "next".
        if let Schedule::At { at } = &request.schedule
            && *at <= request.from
        {
            return Err(Error::failed(format!(
                "one-shot schedule at {at} is not after {}",
                request.from
            )));
        }
        maths::next_run_for_schedule(&request.schedule, request.from).map_err(failure)
    }
    async fn validate_schedule(&self, request: ValidateScheduleRequest) -> Result<()> {
        if request.agent {
            maths::validate_agent_schedule(&request.schedule, request.now)
        } else {
            maths::validate_schedule(&request.schedule, request.now)
        }
        .map_err(failure)
    }
    async fn runs_closer_than(
        &self,
        request: RunsCloserThanRequest,
    ) -> Result<Option<TooFrequent>> {
        let min_gap = chrono::Duration::try_milliseconds(request.min_gap_ms)
            .ok_or_else(|| Error::failed("min_gap_ms out of range"))?;
        Ok(maths::runs_closer_than(
            &request.schedule,
            request.from,
            min_gap,
        ))
    }
    async fn normalize_expression(&self, expression: String) -> Result<String> {
        maths::normalize_expression(&expression).map_err(failure)
    }
    async fn schedule_cron_expression(&self, schedule: Schedule) -> Result<Option<String>> {
        Ok(maths::schedule_cron_expression(&schedule))
    }
    async fn default_delivery(&self, origin: Option<JobOrigin>) -> Result<DeliveryConfig> {
        Ok(validation::default_delivery(origin.as_ref()))
    }
    async fn check_origin_requirements(&self, request: CheckOriginRequest) -> Result<()> {
        validation::check_origin_requirements(
            &request.session_target,
            &request.delivery,
            request.origin.as_ref(),
        )
        .map_err(Error::failed)
    }
    async fn check_patch(&self, request: CheckPatchRequest) -> Result<()> {
        store::check_patch(&request.job, &request.patch).map_err(failure)
    }
    async fn truncate_cron_output(&self, output: String) -> Result<String> {
        Ok(store::truncate_cron_output(&output))
    }
}

pub(super) async fn setup(connection: &Connection) -> Result<()> {
    connection
        .serve_at(
            schedule::OBJECT_PATH.try_into()?,
            ScheduleService::new(connection.clone()),
        )
        .await?;
    Ok(())
}

#[cfg(test)]
#[path = "schedule_members_tests.rs"]
mod members_tests;
#[cfg(test)]
#[path = "schedule_service_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "schedule_clock_tests.rs"]
mod clock_tests;
