//! Module-owned persistence dispatch. Hosts pass configuration and JSON only.
use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;
use tinybus::Connection;
use tinyflows_bus::schedule::{StoreBackend, StoreConfig, StoreRequest};
use tinyflows_drivers::schedule::CronDocuments;
use tinyflows_schedule::{AgentJobSpec, CronJob, CronJobPatch, DeliveryStatus, Schedule};
use tinyflows_sqlite::schedule::{self as sql, CronStoreOptions};

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Operation {
    Add {
        expression: String,
        command: String,
    },
    AddShell {
        name: Option<String>,
        schedule: Schedule,
        command: String,
    },
    AddAgent {
        spec: AgentJobSpec,
    },
    AddFlow {
        flow_id: String,
        schedule: Schedule,
    },
    FindFlow {
        flow_id: String,
    },
    List,
    Get {
        job_id: String,
    },
    Remove {
        job_id: String,
    },
    Clear,
    Dedup,
    Due {
        now: DateTime<Utc>,
    },
    Update {
        job_id: String,
        patch: CronJobPatch,
    },
    RecordLastRun {
        job_id: String,
        finished_at: DateTime<Utc>,
        success: bool,
        output: String,
    },
    Reschedule {
        job: CronJob,
        success: bool,
        output: String,
    },
    RecordRun {
        job_id: String,
        started_at: DateTime<Utc>,
        finished_at: DateTime<Utc>,
        status: String,
        output: Option<String>,
        duration_ms: i64,
        delivery_status: Option<DeliveryStatus>,
    },
    DeleteQueuedRuns {
        job_id: String,
    },
    ListRuns {
        job_id: String,
        limit: usize,
    },
}

pub(super) enum Store {
    Sqlite(CronStoreOptions),
    Documents(CronDocuments),
}
impl Store {
    pub(super) async fn new(connection: Connection, config: StoreConfig) -> Result<Self> {
        Ok(match config.backend {
            StoreBackend::Sqlite { path } => {
                if path.is_empty() {
                    bail!("empty schedule database path");
                }
                Self::Sqlite(CronStoreOptions {
                    db_path: path.into(),
                    max_run_history: config.max_run_history,
                    max_tasks: config.max_tasks,
                })
            }
            StoreBackend::Documents { callback } => Self::Documents(
                CronDocuments::new(Arc::new(
                    super::schedule_documents::Documents::new(connection, callback).await?,
                ))
                .with_limits(config.max_run_history, config.max_tasks),
            ),
        })
    }
    pub(super) async fn request(connection: Connection, request: StoreRequest) -> Result<Value> {
        Self::new(connection, request.storage)
            .await?
            .execute(serde_json::from_value(request.operation)?)
            .await
    }
    async fn execute(&self, operation: Operation) -> Result<Value> {
        // Each backend owns validation, migration, output limits and history pruning.
        macro_rules! dispatch {
            ($method:ident $(, $arg:expr)*) => {
                match self {
                    Self::Sqlite(options) => serde_json::to_value(sql::$method(options $(, $arg)*)?)?,
                    Self::Documents(documents) => serde_json::to_value(documents.$method($($arg),*).await?)?,
                }
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
        })
    }
}

#[async_trait::async_trait]
impl tinyflows_schedule::runtime::JobStore for Store {
    async fn due_jobs(&self, now: DateTime<Utc>) -> Result<Vec<CronJob>> {
        match self {
            Self::Sqlite(options) => sql::due_jobs(options, now),
            Self::Documents(documents) => documents.due_jobs(now).await,
        }
    }
    async fn due_jobs_excluding(
        &self,
        now: DateTime<Utc>,
        excluded: &[String],
    ) -> Result<Vec<CronJob>> {
        match self {
            Self::Sqlite(options) => sql::due_jobs_excluding(options, now, excluded),
            Self::Documents(documents) => documents.due_jobs_excluding(now, excluded).await,
        }
    }
    async fn claim_slot(&self, job: &CronJob, next: DateTime<Utc>) -> Result<()> {
        let claimed = match self {
            Self::Sqlite(options) => sql::claim_slot(options, job, next)?,
            Self::Documents(documents) => documents.claim_slot(job, next).await?,
        };
        anyhow::ensure!(
            claimed,
            "schedule occurrence was removed, edited or already claimed"
        );
        Ok(())
    }
    async fn record_run(&self, record: tinyflows_schedule::runtime::RunRecord) -> Result<()> {
        match self {
            Self::Sqlite(options) => sql::record_claimed_run(options, &record),
            Self::Documents(documents) => documents.record_claimed_run(&record).await,
        }
    }

    async fn complete(
        &self,
        job: &CronJob,
        completion: tinyflows_schedule::runtime::Completion,
    ) -> Result<()> {
        match self {
            Self::Sqlite(options) => sql::complete_run(options, job, &completion),
            Self::Documents(documents) => documents.complete_run(job, &completion).await,
        }
    }
}

#[cfg(test)]
#[path = "schedule_store_tests.rs"]
mod tests;
