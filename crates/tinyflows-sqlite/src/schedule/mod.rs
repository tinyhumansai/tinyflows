//! SQLite persistence for scheduled jobs and their run history.
//!
//! The job/run *model* and next-run computation live in `tinyflows-schedule`;
//! this module is the storage half. Every function takes a
//! [`CronStoreOptions`] (database path, run-history cap, due-job batch size)
//! instead of a host config type, opens the database on demand and creates
//! or upgrades the schema idempotently, so a database written by an older
//! build opens unchanged (`schedule_schema_tests` pins the literal statements).
//!
//! Split by responsibility: [`schema`] owns row mapping and the
//! connection/migration setup, [`jobs`] owns job CRUD, and [`runs`] owns
//! run-history recording, output truncation, and history reads.

use std::path::PathBuf;

mod agent_jobs;
mod jobs;
mod runs;
mod runtime;
pub use runtime::{claim_slot, complete_run};
mod schema;

pub use agent_jobs::{AgentJobSpec, add_agent_job_from_spec};
pub use jobs::{
    add_agent_job, add_agent_job_with_definition, add_flow_schedule_job, add_job, add_shell_job,
    clear_all_jobs, dedup_named_jobs, due_jobs, due_jobs_excluding, find_flow_schedule_job,
    get_job, list_jobs, remove_job, update_job,
};
pub use runs::{
    MAX_CRON_OUTPUT_BYTES, TRUNCATED_OUTPUT_MARKER, delete_queued_runs, list_runs,
    record_claimed_run, record_last_run, record_run, record_run_with_delivery,
    reschedule_after_run,
};
pub use schema::with_connection;

/// What the store needs from its host, in place of a config type.
#[derive(Debug, Clone)]
pub struct CronStoreOptions {
    /// Path of the SQLite file (`jobs.db`); its parent directory is created on
    /// first use.
    pub db_path: PathBuf,
    /// Runs kept per job; older ones are pruned on every insert (min 1).
    pub max_run_history: usize,
    /// Upper bound on the jobs [`due_jobs`] returns per call (min 1).
    pub max_tasks: usize,
}

impl CronStoreOptions {
    /// Options with the default history cap (50) and batch size (64).
    pub fn new(db_path: impl Into<PathBuf>) -> Self {
        Self {
            db_path: db_path.into(),
            max_run_history: 50,
            max_tasks: 64,
        }
    }
}

#[cfg(test)]
#[path = "schedule_agent_floor_tests.rs"]
mod schedule_agent_floor_tests;
#[cfg(test)]
#[path = "schedule_dedup_tests.rs"]
mod schedule_dedup_tests;
#[cfg(test)]
#[path = "schedule_origin_tests.rs"]
mod schedule_origin_tests;
#[cfg(test)]
#[path = "schedule_schema_tests.rs"]
mod schedule_schema_tests;
#[cfg(test)]
#[path = "schedule_tests.rs"]
mod schedule_tests;

#[cfg(test)]
#[path = "policy_tests.rs"]
mod policy_tests;
