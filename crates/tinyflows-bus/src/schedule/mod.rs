//! Transport-only scheduler vocabulary. Jobs and records keep their JSON wire
//! shape without making hosts link scheduler or storage implementation crates.
use serde::{Deserialize, Serialize};

mod frequency;
mod records;
mod requests;
mod store_types;
pub mod types;

pub use frequency::{MIN_AGENT_JOB_INTERVAL, TooFrequent, describe_gap};
pub use records::{Attempt, Completion, RunRecord};
pub use requests::{
    AcknowledgeManualRun, CheckOriginRequest, CheckPatchRequest, DeliveryProjection,
    ManualClaimReply, ManualDelivery, ManualRunHandle, ManualRunOutcome, ManualRunRequest,
    ManualRunState, NextRunRequest, OccurrenceClaim, PrepareManualRun, RunJobNow, RunJobRefusal,
    RunJobReply, RunsCloserThanRequest, StartManualRun, StopManualRun, ValidateScheduleRequest,
    WaitManualRun,
};
pub use store_types::{
    AgentJobSpec, ImportedJob, MAX_CRON_OUTPUT_BYTES, StoreOperation, TRUNCATED_OUTPUT_MARKER,
};
pub use types::{
    ActiveHours, CronJob, CronJobPatch, CronRun, DeliveryConfig, DeliveryStatus, JobOrigin,
    JobPolicy, JobType, Schedule, SessionTarget, delivery_mode,
};

/// The module has one bus identity and separately served authoring/schedule objects.
pub const BUS_NAME: &str = crate::BUS_NAME;
/// Separate scheduler interface in the shared module.
pub const INTERFACE: &str = "ai.tinyhumans.tinyflows.Schedule";
/// Served scheduler object path.
pub const OBJECT_PATH: &str = "/ai/tinyhumans/tinyflows/Schedule";
/// Host interface carrying the context-bound runtime ports.
pub const CALLBACK_INTERFACE: &str = "ai.tinyhumans.tinyflows.ScheduleHost";
/// Version of the scheduler wire vocabulary. 2 removed the module-owned
/// `sqlite` store backend; storage is the host's document port only. 3 moved
/// the job/run/policy records and the callback records into this crate
/// (`types`, `Attempt`/`RunRecord`/`Completion`, `AgentJobSpec`,
/// `StoreOperation`) and added the schedule-maths and validation members and
/// the `set_max_active_jobs`/`import_job`/`import_run` store operations. 4 added
/// optional `config.now` for fixed calendar time on manual schedulers. 5 adds
/// occurrence leases, module-owned manual run admission and explicit
/// send/terminal delivery phases. 6 adds prepared handles and host-owned
/// lifecycle observation for detached manual workers.
pub const CONTRACT_VERSION: u32 = 6;
/// Exact advertised scheduler member set.
pub const METHODS: &[&str] = &[
    members::SCHEDULE_STORE,
    members::START_SCHEDULER,
    members::STOP_SCHEDULER,
    members::TICK_SCHEDULER,
    members::DRAIN_SCHEDULER,
    members::RUN_JOB_NOW,
    members::PREPARE_MANUAL_RUN,
    members::START_MANUAL_RUN,
    members::WAIT_MANUAL_RUN,
    members::STOP_MANUAL_RUN,
    members::ACKNOWLEDGE_MANUAL_RUN,
    members::PARSE_HUMAN_DELAY,
    members::VALIDATE_DELIVERY,
    members::NEXT_RUN,
    members::VALIDATE_SCHEDULE,
    members::RUNS_CLOSER_THAN,
    members::NORMALIZE_EXPRESSION,
    members::SCHEDULE_CRON_EXPRESSION,
    members::DEFAULT_DELIVERY,
    members::CHECK_ORIGIN_REQUIREMENTS,
    members::CHECK_PATCH,
    members::TRUNCATE_CRON_OUTPUT,
];

/// Stable scheduler member names, in `METHODS` order.
pub mod members {
    /// `(StoreRequest) -> JSON`; see `STORE_OPERATIONS`.
    pub const SCHEDULE_STORE: &str = "ScheduleStore";
    /// `(StartScheduler) -> handle: string`.
    pub const START_SCHEDULER: &str = "StartScheduler";
    /// `(handle: string) -> ()`.
    pub const STOP_SCHEDULER: &str = "StopScheduler";
    /// `(handle: string) -> usize`; manual-tick schedulers only.
    pub const TICK_SCHEDULER: &str = "TickScheduler";
    /// `(handle: string) -> ()`; manual-tick schedulers only.
    pub const DRAIN_SCHEDULER: &str = "DrainScheduler";
    /// `(RunJobNow) -> RunJobReply`; durable admission precedes acknowledgement.
    pub const RUN_JOB_NOW: &str = "RunJobNow";
    /// `(PrepareManualRun) -> ManualRunHandle`; creates an inert reservation.
    pub const PREPARE_MANUAL_RUN: &str = "PrepareManualRun";
    /// `(StartManualRun) -> ManualRunState`; starts only a live prepared handle.
    pub const START_MANUAL_RUN: &str = "StartManualRun";
    /// `(WaitManualRun) -> Option<ManualRunState>`; immediate snapshot, absent means quiescent.
    pub const WAIT_MANUAL_RUN: &str = "WaitManualRun";
    /// `(StopManualRun) -> Option<ManualRunState>`; requests stop, absent means quiescent.
    pub const STOP_MANUAL_RUN: &str = "StopManualRun";
    /// `(AcknowledgeManualRun) -> bool`; removes a retained terminal result.
    pub const ACKNOWLEDGE_MANUAL_RUN: &str = "AcknowledgeManualRun";
    /// `(delay: string) -> milliseconds: i64`.
    pub const PARSE_HUMAN_DELAY: &str = "ParseHumanDelay";
    /// `(DeliveryConfig, Option<JobOrigin>, Option<Vec<String>>) -> ()`.
    pub const VALIDATE_DELIVERY: &str = "ValidateDelivery";
    /// `(NextRunRequest) -> DateTime<Utc>`.
    pub const NEXT_RUN: &str = "NextRun";
    /// `(ValidateScheduleRequest) -> ()`.
    pub const VALIDATE_SCHEDULE: &str = "ValidateSchedule";
    /// `(RunsCloserThanRequest) -> Option<TooFrequent>`.
    pub const RUNS_CLOSER_THAN: &str = "RunsCloserThan";
    /// `(expression: string) -> string`: POSIX crontab weekdays into the
    /// `cron` crate's numbering; errors on a malformed expression.
    pub const NORMALIZE_EXPRESSION: &str = "NormalizeExpression";
    /// `(Schedule) -> Option<string>`: the cron expression, if it has one.
    pub const SCHEDULE_CRON_EXPRESSION: &str = "ScheduleCronExpression";
    /// `(Option<JobOrigin>) -> DeliveryConfig`: origin delivery with an
    /// origin, proactive without.
    pub const DEFAULT_DELIVERY: &str = "DefaultDelivery";
    /// `(CheckOriginRequest) -> ()`.
    pub const CHECK_ORIGIN_REQUIREMENTS: &str = "CheckOriginRequirements";
    /// `(CheckPatchRequest) -> ()`.
    pub const CHECK_PATCH: &str = "CheckPatch";
    /// `(output: string) -> string`, bounded to `MAX_CRON_OUTPUT_BYTES`.
    pub const TRUNCATE_CRON_OUTPUT: &str = "TruncateCronOutput";
}

/// A host callback object bound to the owning agent/tenant context.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CallbackAddress {
    /// Bus name of the host callback service.
    pub destination: String,
    /// Callback object bound to one agent/tenant context.
    pub object_path: String,
}

/// Whether the module runs its poll loop, or the host drives individual ticks.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StartScheduler {
    /// Process-wide claim namespace; equal keys share job and loop claims.
    pub scope: String,
    /// Context-bound I/O callbacks used for every scheduled run.
    pub callback: CallbackAddress,
    /// `poll_secs`, `max_concurrent`, `retries`, `backoff_ms`; omitted keys use defaults.
    /// Manual schedulers additionally accept `now` (RFC3339), fixing calendar
    /// time for due selection, run records and rescheduling without changing
    /// the machine clock. Omit it to use system time. Automatic loops refuse it.
    pub config: serde_json::Value,
    #[serde(default)]
    /// True to drive ticks/drain through calls; false starts the module-owned loop.
    pub manual_ticks: bool,
    /// Module-owned store; absent retains the original storage callback seam.
    #[serde(default)]
    pub storage: Option<StoreConfig>,
}

/// Stable host callback member names. The host owns execution policy and I/O;
/// the module computes claims, retry timing, completion and rescheduling.
pub mod callbacks {
    /// `(now: RFC3339) -> jobs: JSON[]`.
    pub const DUE_JOBS: &str = "DueJobs";
    /// `(now: RFC3339, excluded: string[]) -> jobs: JSON[]`. Apply exclusions
    /// before the batch limit; required when an active job defers a due slot.
    pub const DUE_JOBS_EXCLUDING: &str = "DueJobsExcluding";
    /// `(captured_job, owner, now, lease_ms) -> Option<OccurrenceClaim>`.
    pub const CLAIM_DUE_OCCURRENCE: &str = "ClaimDueOccurrence";
    /// `(job, owner, now, lease_ms) -> ManualClaimReply`; manual force-run.
    pub const CLAIM_MANUAL_OCCURRENCE: &str = "ClaimManualOccurrence";
    /// `(receipt, now, lease_ms) -> bool`.
    pub const RENEW_OCCURRENCE: &str = "RenewOccurrence";
    /// `(receipt, now) -> bool`.
    pub const RELEASE_OCCURRENCE: &str = "ReleaseOccurrence";
    /// `(receipt, record) -> bool`; durable queued history before acknowledgement.
    pub const QUEUE_OCCURRENCE: &str = "QueueOccurrence";
    /// `(receipt, record, completion) -> bool`; atomic terminal commit.
    pub const COMPLETE_OCCURRENCE: &str = "CompleteOccurrence";
    /// `(job: JSON, run_id: string) -> {success, output, retryable}`.
    pub const EXECUTE: &str = "Execute";
    /// `(job: JSON, run_id: string, attempt: JSON) -> ()` after final failure.
    pub const FAILED: &str = "Failed";
    /// `(job, run_id, attempt) -> delivery-status`; performs the one actual send.
    pub const SEND: &str = "Send";
    /// `(DeliveryProjection) -> ()`; observes final state and never sends.
    pub const DELIVER: &str = "Deliver";
    /// `(captured_job: CronJob, run_id: string) -> Attempt`.
    pub const SCHEDULE_TRIGGER: &str = "ScheduleTrigger";
}

/// Storage selection and limits supplied by the scope-owning host.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoreConfig {
    /// Backend already bound to the correct workspace or agent scope.
    pub backend: StoreBackend,
    /// Run-history cap; defaults to 50 and is clamped to at least one.
    #[serde(default = "history_limit")]
    pub max_run_history: usize,
    /// Due-job batch limit; defaults to 64 and is clamped to at least one.
    #[serde(default = "task_limit")]
    pub max_tasks: usize,
}
fn history_limit() -> usize {
    50
}
fn task_limit() -> usize {
    64
}

/// A context-bound host document port. The module owns no storage of its own:
/// it runs `CronDocuments` over the host's scoped `DocumentStore`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StoreBackend {
    /// The module owns CronDocuments queries, migrations and CAS loops.
    Documents {
        /// Host callback object implementing the minimal schedule document port.
        callback: CallbackAddress,
    },
}

/// One schedule persistence operation. Payloads retain existing job/run JSON
/// vocabulary without making hosts import implementation model crates.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoreRequest {
    /// Context-bound backend and persistence limits.
    pub storage: StoreConfig,
    /// Operation name and arguments; see `STORE_OPERATIONS`.
    pub operation: serde_json::Value,
}

/// Supported `operation.op` values; `StoreOperation` is their typed form. Other fields use job/spec/patch
/// JSON shapes. Run timestamps are RFC3339 and durations are milliseconds.
pub const STORE_OPERATIONS: &[&str] = &[
    "add",
    "add_shell",
    "add_agent",
    "add_flow",
    "find_flow",
    "list",
    "get",
    "remove",
    "clear",
    "dedup",
    "due",
    "update",
    "record_last_run",
    "reschedule",
    "record_run",
    "delete_queued_runs",
    "list_runs",
    "set_max_active_jobs",
    "import_job",
    "import_run",
    "migrate",
];

/// Context-bound document callback interface. The only member is `Document`;
/// it accepts a JSON request with `op` and the corresponding core-port fields.
/// Operations are ensure/get/put/delete/query/count/delete_where. Replies are
/// `{value: JSON}` or `{error: {kind, message}}`, preserving conflict kinds.
/// Core-port JSON uses the tinystoragedrivers-core vocabulary. The host
/// delegates mechanically to its scoped DocumentStore and owns no cron policy.
pub const DOCUMENT_INTERFACE: &str = "ai.tinyhumans.tinyflows.ScheduleDocuments";
/// Exact document callback member used by the module.
pub const DOCUMENT_METHOD: &str = "Document";

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
#[cfg(test)]
#[path = "wire_tests.rs"]
mod wire_tests;
