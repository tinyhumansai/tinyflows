//! Store-wire vocabulary: the agent-job spec, the typed form of a store
//! operation, and the output bound every store applies.
use super::types::{
    CronJob, CronJobPatch, CronRun, DeliveryConfig, DeliveryStatus, JobOrigin, Schedule,
    SessionTarget,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Largest job output, in bytes, a store keeps in `last_output` or a run;
/// longer output is cut at a char boundary and ends with
/// [`TRUNCATED_OUTPUT_MARKER`]. The cut itself runs in the module
/// (`TruncateCronOutput`, and inside every store write).
pub const MAX_CRON_OUTPUT_BYTES: usize = 16 * 1024;
/// Suffix appended to output cut to [`MAX_CRON_OUTPUT_BYTES`].
pub const TRUNCATED_OUTPUT_MARKER: &str = "\n...[truncated]";

/// Everything an agent job is created with. [`AgentJobSpec::new`] gives an
/// enabled, isolated job with no name, model, definition or origin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentJobSpec {
    /// Display name.
    pub name: Option<String>,
    /// When the job fires.
    pub schedule: Schedule,
    /// Prompt the agent runs.
    pub prompt: String,
    /// Which session the run uses.
    pub session_target: SessionTarget,
    /// Model override.
    pub model: Option<String>,
    /// `None` stores [`DeliveryConfig::default`].
    pub delivery: Option<DeliveryConfig>,
    /// Remove the job after a successful run.
    pub delete_after_run: bool,
    /// Built-in agent definition to run with.
    pub agent_id: Option<String>,
    /// Inserted in this state in one write, so an opt-in job is never
    /// briefly enabled.
    pub enabled: bool,
    /// The conversation the job was created from.
    pub origin: Option<JobOrigin>,
}

impl AgentJobSpec {
    /// An enabled, isolated job with no name, model, definition or origin.
    pub fn new(schedule: Schedule, prompt: impl Into<String>) -> Self {
        Self {
            name: None,
            schedule,
            prompt: prompt.into(),
            session_target: SessionTarget::default(),
            model: None,
            delivery: None,
            delete_after_run: false,
            agent_id: None,
            enabled: true,
            origin: None,
        }
    }
}

/// What `import_job` did with a job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportedJob {
    /// The id the job has in this store. It differs from the imported job's own
    /// id for a flow-schedule job; its runs are imported under this id.
    pub id: String,
    /// Whether the call wrote the job. `false` when the store already held a
    /// job under `id`, which is left exactly as it was.
    pub written: bool,
}

/// Typed form of `StoreRequest::operation`. It serializes to exactly the
/// `{"op": ..., ...}` JSON that `STORE_OPERATIONS` documents, so a host may
/// build the untyped value from this or keep sending hand-built JSON.
///
/// Reply (the `Value` a `ScheduleStore` call returns), per operation:
/// `add`/`add_shell`/`add_agent`/`add_flow`/`get`/`update` a [`CronJob`];
/// `find_flow` an `Option<CronJob>`; `list`/`due` a `Vec<CronJob>`;
/// `remove`/`set_max_active_jobs` `null`; `clear`/`dedup`/`delete_queued_runs`
/// a count; `list_runs` a `Vec<CronRun>`; `import_job` an [`ImportedJob`];
/// `import_run` a bool (written); the rest follow the document store's
/// `record_*` results.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum StoreOperation {
    /// A shell job from a bare cron expression.
    Add {
        /// Cron expression.
        expression: String,
        /// Shell command.
        command: String,
    },
    /// A shell job.
    AddShell {
        /// Display name.
        name: Option<String>,
        /// When the job fires.
        schedule: Schedule,
        /// Shell command.
        command: String,
    },
    /// An agent job from a full spec.
    AddAgent {
        /// The job to create.
        spec: AgentJobSpec,
    },
    /// The schedule-trigger job bound to a flow.
    AddFlow {
        /// Bound flow.
        flow_id: String,
        /// When it fires.
        schedule: Schedule,
    },
    /// Look up a flow's schedule-trigger job.
    FindFlow {
        /// Bound flow.
        flow_id: String,
    },
    /// Every job.
    List,
    /// One job by id.
    Get {
        /// Job id.
        job_id: String,
    },
    /// Remove a job and its history.
    Remove {
        /// Job id.
        job_id: String,
    },
    /// Remove every job.
    Clear,
    /// Collapse same-named jobs.
    Dedup,
    /// Enabled jobs due at `now`.
    Due {
        /// Selection instant.
        now: DateTime<Utc>,
    },
    /// Apply a patch.
    Update {
        /// Job id.
        job_id: String,
        /// Fields to change.
        patch: CronJobPatch,
    },
    /// Record last-run bookkeeping.
    RecordLastRun {
        /// Job id.
        job_id: String,
        /// Finish instant.
        finished_at: DateTime<Utc>,
        /// Outcome.
        success: bool,
        /// Result text.
        output: String,
    },
    /// Reschedule after a run.
    Reschedule {
        /// The job as it ran.
        job: CronJob,
        /// Outcome.
        success: bool,
        /// Result text.
        output: String,
    },
    /// Append a run to a job's history.
    RecordRun {
        /// Job id.
        job_id: String,
        /// Start instant.
        started_at: DateTime<Utc>,
        /// Finish instant.
        finished_at: DateTime<Utc>,
        /// `ok`, `error` or `skipped`.
        status: String,
        /// Result text.
        output: Option<String>,
        /// Run duration in milliseconds.
        duration_ms: i64,
        /// Delivery outcome.
        delivery_status: Option<DeliveryStatus>,
    },
    /// Drop queued (not yet started) runs of a job.
    DeleteQueuedRuns {
        /// Job id.
        job_id: String,
    },
    /// A job's run history, newest first.
    ListRuns {
        /// Job id.
        job_id: String,
        /// Maximum rows.
        limit: usize,
    },
    /// Install or clear the scope's enabled-job cap (`null` clears it).
    SetMaxActiveJobs {
        /// New cap; zero is refused.
        limit: Option<usize>,
    },
    /// Import a job from a retired store, keeping its history fields.
    ImportJob {
        /// The job to import.
        job: CronJob,
    },
    /// Import one run of an already imported job.
    ImportRun {
        /// The run to import.
        run: CronRun,
    },
    /// Apply the module-owned v1 -> v2 schedule migration in this callback scope.
    Migrate,
}
