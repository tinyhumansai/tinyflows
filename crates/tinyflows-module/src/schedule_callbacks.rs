//! Adapt a context-bound TinyBus host callback object to scheduler ports.
use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Serialize, de::DeserializeOwned};
use std::sync::Arc;
use tinybus::Connection;
use tinyflows_bus::schedule::{CALLBACK_INTERFACE, CallbackAddress, callbacks};
use tinyflows_schedule::{CronJob, DeliveryStatus, runtime::*};

pub(super) struct HostCallbacks {
    connection: Connection,
    address: CallbackAddress,
}
impl HostCallbacks {
    pub(super) fn ports(connection: Connection, address: CallbackAddress) -> RuntimePorts {
        let host = Arc::new(Self {
            connection,
            address,
        });
        RuntimePorts {
            store: host.clone(),
            executor: host.clone(),
            delivery: host.clone(),
            triggers: host,
            clock: Arc::new(SystemClock),
        }
    }
    async fn call<A: Serialize + Send + Sync, R: DeserializeOwned>(
        &self,
        member: &str,
        args: &A,
    ) -> Result<R> {
        let proxy = self.connection.proxy(
            &self.address.destination,
            &self.address.object_path,
            CALLBACK_INTERFACE,
        )?;
        Ok(proxy.call(member, args).await?)
    }
}
#[async_trait]
impl JobStore for HostCallbacks {
    async fn due_jobs(&self, now: DateTime<Utc>) -> Result<Vec<CronJob>> {
        self.call(callbacks::DUE_JOBS, &(now,)).await
    }
    async fn due_jobs_excluding(
        &self,
        now: DateTime<Utc>,
        excluded: &[String],
    ) -> Result<Vec<CronJob>> {
        self.call(callbacks::DUE_JOBS_EXCLUDING, &(now, excluded))
            .await
    }
    async fn claim_slot(&self, job: &CronJob, next_run: DateTime<Utc>) -> Result<()> {
        self.call(callbacks::CLAIM_SLOT, &(job, next_run)).await
    }
    async fn record_run(&self, record: RunRecord) -> Result<()> {
        self.call(callbacks::RECORD_RUN, &(record,)).await
    }
    async fn complete(&self, job: &CronJob, completion: Completion) -> Result<()> {
        self.call(callbacks::COMPLETE, &(job, completion)).await
    }
}
#[async_trait]
impl JobExecutor for HostCallbacks {
    async fn execute(&self, job: &CronJob, run_id: &str) -> Result<Attempt> {
        self.call(callbacks::EXECUTE, &(job, run_id)).await
    }
    async fn failed(&self, job: &CronJob, run_id: &str, attempt: &Attempt) {
        if let Err(error) = self
            .call::<_, ()>(callbacks::FAILED, &(job, run_id, attempt))
            .await
        {
            tracing::warn!(%error, "scheduler failure notification failed");
        }
    }
}
#[async_trait]
impl RunDelivery for HostCallbacks {
    async fn deliver(
        &self,
        job: &CronJob,
        run_id: &str,
        attempt: &Attempt,
    ) -> Result<DeliveryStatus> {
        self.call(callbacks::DELIVER, &(job, run_id, attempt)).await
    }
}
#[async_trait]
impl TriggerSink for HostCallbacks {
    async fn schedule(&self, flow_id: &str, run_id: &str) -> Result<Attempt> {
        self.call(callbacks::SCHEDULE_TRIGGER, &(flow_id, run_id))
            .await
    }
}
