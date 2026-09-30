//! Scheduling model and next-run computation for timed jobs.
//!
//! * [`types`] — [`Schedule`], [`ActiveHours`] and the job/run records a host
//!   persists. Their serde shapes are a wire contract (a host's job store and
//!   RPC surface carry them), pinned by `wire_tests`.
//! * [`schedule`] — next-run computation, validation, cron-expression
//!   normalisation and the minimum-cadence check.
//!
//! No runtime, storage or configuration lives here; a host supplies those.

pub mod schedule;
pub mod types;

pub use schedule::{
    MIN_AGENT_JOB_INTERVAL, TooFrequent, next_run_for_schedule, normalize_expression,
    runs_closer_than, schedule_cron_expression, validate_agent_schedule, validate_schedule,
};
pub use types::{
    ActiveHours, CronJob, CronJobPatch, CronRun, DeliveryConfig, JobType, Schedule, SessionTarget,
};

#[cfg(test)]
#[path = "wire_tests.rs"]
mod wire_tests;
