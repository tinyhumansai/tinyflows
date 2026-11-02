//! Scheduling model and next-run computation for timed jobs.
//!
//! * [`types`] — [`Schedule`], [`ActiveHours`] and the job/run records a host
//!   persists. Their serde shapes are a wire contract (a host's job store and
//!   RPC surface carry them), pinned by `wire_tests`.
//! * [`schedule`] — next-run computation, validation, cron-expression
//!   normalisation (POSIX crontab weekdays into the `cron` crate's numbering)
//!   and the minimum-cadence check.
//!
//! * [`store`] — what every job store shares: [`AgentJobSpec`] and the
//!   bound on stored run output.
//!
//! [`runtime`] owns dispatch, retries and rescheduling over host I/O ports.

mod posix_weekday;
pub mod runtime;
pub mod schedule;
pub mod store;
pub mod types;
pub mod validation;

pub use schedule::{
    MIN_AGENT_JOB_INTERVAL, TooFrequent, next_run_for_schedule, normalize_expression,
    runs_closer_than, schedule_cron_expression, validate_agent_schedule, validate_schedule,
};
pub use store::{
    AgentJobSpec, ImportedJob, MAX_CRON_OUTPUT_BYTES, TRUNCATED_OUTPUT_MARKER, check_patch,
    truncate_cron_output,
};
pub use tinyflows_bus::schedule::{ChannelConversationIdentity, ChannelSessionIdentity};
pub use types::{
    ActiveHours, CronJob, CronJobPatch, CronRun, DeliveryConfig, DeliveryStatus, JobOrigin,
    JobPolicy, JobType, Schedule, SessionTarget, delivery_mode,
};

#[cfg(test)]
#[path = "wire_tests.rs"]
mod wire_tests;
