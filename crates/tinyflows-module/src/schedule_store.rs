//! Module-owned persistence dispatch. Hosts pass configuration and JSON only.
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::sync::Arc;
use tinybus::Connection;
use tinyflows_bus::schedule::{
    OccurrenceClaim, RunJobRefusal, StoreBackend, StoreConfig, StoreOperation as Operation,
    StoreRequest,
};
use tinyflows_drivers::schedule::{ClaimedOccurrence, CronDocuments};
use tinyflows_schedule::{
    CronJob,
    runtime::{Completion, RunRecord},
};
use tinystoragedrivers_core::{MigrateOptions, Registry, Scope};

/// The module's schedule store: `CronDocuments` over the host's document port.
pub(super) struct Store(CronDocuments);
impl Store {
    pub(super) async fn new(connection: Connection, config: StoreConfig) -> Result<Self> {
        let StoreBackend::Documents { callback } = config.backend;
        Ok(Self(
            CronDocuments::new(Arc::new(
                super::schedule_documents::Documents::new(connection, callback).await?,
            ))
            .with_limits(config.max_run_history, config.max_tasks),
        ))
    }
    pub(super) async fn request(connection: Connection, request: StoreRequest) -> Result<Value> {
        let StoreRequest { storage, operation } = request;
        let operation: Operation = serde_json::from_value(operation)?;
        if matches!(operation, Operation::Migrate) {
            return Self::migrate(connection, storage).await;
        }
        Self::new(connection, storage)
            .await?
            .execute(operation)
            .await
    }
    async fn migrate(connection: Connection, config: StoreConfig) -> Result<Value> {
        let StoreBackend::Documents { callback } = config.backend;
        let documents =
            Arc::new(super::schedule_documents::Documents::new(connection, callback).await?);
        documents.require_record_metadata()?;
        let _guard = super::migration_lock::schedule().await;
        let storage = tinyflows_drivers::document_only_scope(Scope::local(), documents.clone())?;
        CronDocuments::new(documents)
            .ensure()
            .await
            .map_err(anyhow::Error::msg)?;
        let registry = Registry::compose([tinyflows_drivers::cron_storage_schema()])?;
        let report = registry
            .migrate(&storage, &MigrateOptions::default())
            .await?;
        let counter_imported = tinyflows_drivers::import_legacy_cron_counters(&storage).await?;
        Ok(serde_json::json!({
            "steps_run": report.ran.len(),
            "already_done": report.already_done,
            "counter_imported": counter_imported,
        }))
    }
    async fn execute(&self, operation: Operation) -> Result<Value> {
        // The store owns validation, migration, output limits and history pruning.
        let documents = &self.0;
        macro_rules! dispatch {
            ($method:ident $(, $arg:expr)*) => {
                serde_json::to_value(documents.$method($($arg),*).await?)?
            };
        }
        Ok(match operation {
            Operation::Add {
                expression,
                command,
            } => dispatch!(add_job, &expression, &command),
            Operation::AddShell {
                name,
                schedule,
                command,
            } => dispatch!(add_shell_job, name, schedule, &command),
            Operation::AddAgent { spec } => dispatch!(add_agent_job_from_spec, spec),
            Operation::AddFlow { flow_id, schedule } => {
                dispatch!(add_flow_schedule_job, &flow_id, schedule)
            }
            Operation::FindFlow { flow_id } => dispatch!(find_flow_schedule_job, &flow_id),
            Operation::List => dispatch!(list_jobs),
            Operation::Get { job_id } => dispatch!(get_job, &job_id),
            Operation::Remove { job_id } => dispatch!(remove_job, &job_id),
            Operation::Clear => dispatch!(clear_all_jobs),
            Operation::Dedup => dispatch!(dedup_named_jobs),
            Operation::Due { now } => dispatch!(due_jobs, now),
            Operation::Update { job_id, patch } => dispatch!(update_job, &job_id, patch),
            Operation::RecordLastRun {
                job_id,
                finished_at,
                success,
                output,
            } => dispatch!(record_last_run, &job_id, finished_at, success, &output),
            Operation::Reschedule {
                job,
                success,
                output,
            } => dispatch!(reschedule_after_run, &job, success, &output),
            Operation::RecordRun {
                job_id,
                started_at,
                finished_at,
                status,
                output,
                duration_ms,
                delivery_status,
            } => dispatch!(
                record_run_with_delivery,
                &job_id,
                started_at,
                finished_at,
                &status,
                output.as_deref(),
                duration_ms,
                delivery_status
            ),
            Operation::DeleteQueuedRuns { job_id } => dispatch!(delete_queued_runs, &job_id),
            Operation::ListRuns { job_id, limit } => dispatch!(list_runs, &job_id, limit),
            Operation::SetMaxActiveJobs { limit } => dispatch!(set_max_active_jobs, limit),
            Operation::ImportJob { job } => dispatch!(import_job, &job),
            Operation::ImportRun { run } => dispatch!(import_run, &run),
            Operation::Migrate => anyhow::bail!("migration operation was not dispatched"),
        })
    }
}

#[async_trait::async_trait]
impl tinyflows_schedule::runtime::JobStore for Store {
    async fn validate_manual_claim_support(&self) -> Result<()> {
        self.0.require_manual_claim_support()
    }

    async fn due_jobs(&self, now: DateTime<Utc>) -> Result<Vec<CronJob>> {
        self.0.due_jobs(now).await
    }
    async fn due_jobs_excluding(
        &self,
        now: DateTime<Utc>,
        excluded: &[String],
    ) -> Result<Vec<CronJob>> {
        self.0.due_jobs_excluding(now, excluded).await
    }
    async fn claim_due_occurrence(
        &self,
        job: &CronJob,
        owner: &str,
        now: DateTime<Utc>,
        lease: chrono::Duration,
    ) -> Result<Option<OccurrenceClaim>> {
        let Some(claim) = self.0.claim_due_job_for(job, owner, now, lease).await? else {
            return Ok(None);
        };
        Ok(Some(occurrence_claim(claim)?))
    }
    async fn claim_manual_occurrence(
        &self,
        job: &CronJob,
        owner: &str,
        now: DateTime<Utc>,
        lease: chrono::Duration,
    ) -> Result<Result<OccurrenceClaim, RunJobRefusal>> {
        let Some(claim) = self.0.claim_job_now(job, owner, now, lease).await? else {
            let current = self.0.find_job(&job.id).await?;
            let changed = current.is_none_or(|current| {
                current.created_at != job.created_at
                    || current.claim_generation != job.claim_generation
                    || !current.same_execution_definition(job)
                    || current.schedule != job.schedule
                    || current.next_run != job.next_run
            });
            return Ok(Err(if changed {
                RunJobRefusal::DefinitionChanged
            } else {
                RunJobRefusal::AlreadyRunning
            }));
        };
        Ok(Ok(occurrence_claim(claim)?))
    }
    async fn find_owned_manual_claim(
        &self,
        job: &CronJob,
        owner: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<OccurrenceClaim>> {
        self.0
            .reconcile_manual_claim_admission(job, owner, now)
            .await?
            .map(occurrence_claim)
            .transpose()
    }
    async fn renew_occurrence(
        &self,
        claim: &OccurrenceClaim,
        now: DateTime<Utc>,
        lease: chrono::Duration,
    ) -> Result<bool> {
        self.0.renew_claim(&decode_claim(claim)?, now, lease).await
    }
    async fn release_occurrence(
        &self,
        claim: &OccurrenceClaim,
        now: DateTime<Utc>,
    ) -> Result<bool> {
        self.0.release_claim(&decode_claim(claim)?, now).await
    }
    async fn queue_occurrence(&self, claim: &OccurrenceClaim, record: RunRecord) -> Result<bool> {
        self.0
            .queue_claimed_run(&decode_claim(claim)?, record.started_at)
            .await
    }
    async fn complete_occurrence(
        &self,
        claim: &OccurrenceClaim,
        record: RunRecord,
        completion: Completion,
    ) -> Result<bool> {
        let claim = decode_claim(claim)?;
        let completed = if completion.advance_schedule {
            self.0
                .complete_claim(
                    &claim,
                    record.finished_at,
                    completion.success,
                    &completion.output,
                    record.delivery_status,
                )
                .await?
        } else {
            self.0
                .complete_manual_claim(
                    &claim,
                    record.finished_at,
                    completion.success,
                    &completion.output,
                    record.delivery_status,
                )
                .await?
        };
        if completed && completion.remove {
            self.0.remove_completed_claim(&claim).await?;
        }
        if !completed {
            let current = self.0.find_job(&claim.job().id).await?;
            anyhow::ensure!(
                current.is_some_and(|current| current.created_at == claim.job().created_at),
                "removed job creation before terminal commit"
            );
        }
        Ok(completed)
    }
}

fn occurrence_claim(claim: ClaimedOccurrence) -> Result<OccurrenceClaim> {
    Ok(OccurrenceClaim {
        receipt: serde_json::to_string(&claim)?,
        job: claim.job().clone(),
        occurrence: claim.occurrence(),
        started_at: claim.started_at(),
        owner: claim.owner().to_owned(),
        run_id: None,
    })
}

fn decode_claim(claim: &OccurrenceClaim) -> Result<ClaimedOccurrence> {
    let decoded: ClaimedOccurrence = serde_json::from_str(&claim.receipt)?;
    anyhow::ensure!(
        decoded.job().id == claim.job.id
            && decoded.owner() == claim.owner
            && decoded.occurrence() == claim.occurrence
            && decoded.started_at() == claim.started_at,
        "occurrence receipt does not match its captured metadata"
    );
    Ok(decoded)
}

#[cfg(test)]
#[path = "schedule_store_tests.rs"]
mod tests;
