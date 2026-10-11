//! Pieces every job store shares, whatever it stores into: the spec an agent
//! job is created from and the bound on stored run output.
//!
//! `tinyflows-sqlite` and `tinyflows-drivers` both persist [`CronJob`]s and
//! [`CronRun`]s; keeping these here means the two stores cannot drift on
//! what an agent job defaults to or how much output a run keeps.
//!
//! [`CronJob`]: crate::CronJob
//! [`CronRun`]: crate::CronRun

use crate::{CronJob, CronJobPatch, DeliveryConfig, JobOrigin, JobType, Schedule, SessionTarget};

/// Largest job output, in bytes, a store keeps in `last_output` or a run;
/// longer output is cut at a char boundary and ends with
/// [`TRUNCATED_OUTPUT_MARKER`].
pub const MAX_CRON_OUTPUT_BYTES: usize = 16 * 1024;
/// Suffix appended to output cut to [`MAX_CRON_OUTPUT_BYTES`].
pub const TRUNCATED_OUTPUT_MARKER: &str = "\n...[truncated]";

/// Everything an agent job is created with. [`AgentJobSpec::new`] gives an
/// enabled, isolated job with no name, model, definition or origin.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AgentJobSpec {
    pub name: Option<String>,
    pub schedule: Schedule,
    pub prompt: String,
    pub session_target: SessionTarget,
    pub model: Option<String>,
    /// `None` stores [`DeliveryConfig::default`].
    pub delivery: Option<DeliveryConfig>,
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

/// Refuses a patch no store may apply to `job`.
///
/// A flow-schedule job's `command` is the id of the flow it fires, and each
/// flow has at most one such job: the SQLite store keeps them unique by
/// `command`, the document store keys them `flow:<flow_id>`. Re-targeting
/// one by patching `command` would leave the job filed under its old flow,
/// so a later registration for the new flow would add a second job for it.
/// Remove the job and register one for the other flow instead. Setting
/// `command` to its current value is allowed.
///
/// # Errors
///
/// When `patch` changes a flow-schedule job's `command`.
pub fn check_patch(job: &CronJob, patch: &CronJobPatch) -> anyhow::Result<()> {
    if job.job_type == JobType::Flow
        && patch
            .command
            .as_ref()
            .is_some_and(|command| *command != job.command)
    {
        anyhow::bail!(
            "Cron job '{}' fires flow '{}'; its command cannot be changed. Remove it and \
             register a schedule job for the other flow instead",
            job.id,
            job.command
        );
    }
    Ok(())
}

/// `output` bounded to [`MAX_CRON_OUTPUT_BYTES`]: unchanged when it fits,
/// otherwise cut at a char boundary and ended with [`TRUNCATED_OUTPUT_MARKER`].
pub fn truncate_cron_output(output: &str) -> String {
    if output.len() <= MAX_CRON_OUTPUT_BYTES {
        return output.to_string();
    }

    if MAX_CRON_OUTPUT_BYTES <= TRUNCATED_OUTPUT_MARKER.len() {
        return TRUNCATED_OUTPUT_MARKER.to_string();
    }

    let mut cutoff = MAX_CRON_OUTPUT_BYTES - TRUNCATED_OUTPUT_MARKER.len();
    while cutoff > 0 && !output.is_char_boundary(cutoff) {
        cutoff -= 1;
    }

    let mut truncated = output[..cutoff].to_string();
    truncated.push_str(TRUNCATED_OUTPUT_MARKER);
    truncated
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
