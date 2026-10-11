//! Transport-only scheduler vocabulary. Jobs and records keep their JSON wire
//! shape without making hosts link scheduler or storage implementation crates.
use serde::{Deserialize, Serialize};

/// The module has one bus identity and separately served authoring/schedule objects.
pub const BUS_NAME: &str = crate::BUS_NAME;
/// Separate scheduler interface in the shared module.
pub const INTERFACE: &str = "ai.tinyhumans.tinyflows.Schedule";
/// Served scheduler object path.
pub const OBJECT_PATH: &str = "/ai/tinyhumans/tinyflows/Schedule";
/// Host interface carrying the context-bound runtime ports.
pub const CALLBACK_INTERFACE: &str = "ai.tinyhumans.tinyflows.ScheduleHost";
/// Version of the scheduler wire vocabulary.
pub const CONTRACT_VERSION: u32 = 1;
/// Exact advertised scheduler member set.
pub const METHODS: &[&str] = &[
    "ScheduleStore",
    "StartScheduler",
    "StopScheduler",
    "TickScheduler",
    "DrainScheduler",
    "ExecuteJob",
    "ParseHumanDelay",
    "ValidateDelivery",
];

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
    /// `(job: JSON, next_run: RFC3339) -> ()`: claim or return an error.
    /// Preserve concurrent edits; disable one-shots durably before success.
    pub const CLAIM_SLOT: &str = "ClaimSlot";
    /// `(record: JSON) -> ()`; fields match runtime run records.
    pub const RECORD_RUN: &str = "RecordRun";
    /// `(job: JSON, completion: JSON) -> ()`; apply the computed mutation.
    pub const COMPLETE: &str = "Complete";
    /// `(job: JSON, run_id: string) -> {success, output, retryable}`.
    pub const EXECUTE: &str = "Execute";
    /// `(job: JSON, run_id: string, attempt: JSON) -> ()` after final failure.
    pub const FAILED: &str = "Failed";
    /// `(job: JSON, run_id: string, attempt: JSON) -> delivery-status string.
    pub const DELIVER: &str = "Deliver";
    /// `(flow_id: string, run_id: string) -> {success, output, retryable}`.
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

/// Module-owned SQLite or a context-bound host document port.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StoreBackend {
    /// The module opens and migrates this SQLite file.
    Sqlite {
        /// Scoped database path chosen by the host.
        path: String,
    },
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

/// Supported `operation.op` values. Other fields use existing job/spec/patch
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
];

/// Context-bound document callback interface. The only member is `Document`;
/// it accepts a JSON request with `op` and the corresponding core-port fields.
/// Operations are ensure/get/put/delete/query/count/delete_where. Replies are
/// `{value: JSON}` or `{error: {kind, message}}`, preserving conflict kinds.
/// Core-port JSON uses the tinystoragedrivers-core v0.5 vocabulary. The host
/// delegates mechanically to its scoped DocumentStore and owns no cron policy.
pub const DOCUMENT_INTERFACE: &str = "ai.tinyhumans.tinyflows.ScheduleDocuments";
/// Exact document callback member used by the module.
pub const DOCUMENT_METHOD: &str = "Document";
