//! Applying a [`CronJobPatch`] to a job, as the SQLite store applies it.

use anyhow::Result;
use chrono::Utc;
use tinyflows_schedule::{
    CronJob, CronJobPatch, JobType, next_run_for_schedule, schedule_cron_expression,
    validate_agent_schedule, validate_schedule,
};

/// `job` with `patch` applied, exactly as the SQLite store applies it.
pub(super) fn apply_patch(mut job: CronJob, patch: CronJobPatch) -> Result<CronJob> {
    tinyflows_schedule::check_patch(&job, &patch)?;
    let was_enabled = job.enabled;
    let mut schedule_changed = false;
    if let Some(schedule) = patch.schedule {
        // The agent-only floor applies whenever the schedule is (re)set.
        match job.job_type {
            JobType::Agent => validate_agent_schedule(&schedule, Utc::now())?,
            JobType::Shell | JobType::Flow => validate_schedule(&schedule, Utc::now())?,
        }
        job.schedule = schedule;
        job.expression = schedule_cron_expression(&job.schedule).unwrap_or_default();
        schedule_changed = true;
    }
    if let Some(command) = patch.command {
        job.command = command;
    }
    if let Some(prompt) = patch.prompt {
        job.prompt = Some(prompt);
    }
    if let Some(name) = patch.name {
        job.name = Some(name);
    }
    if let Some(enabled) = patch.enabled {
        job.enabled = enabled;
    }
    if let Some(delivery) = patch.delivery {
        job.delivery = delivery;
    }
    if let Some(model) = patch.model {
        job.model = Some(model);
    }
    if let Some(target) = patch.session_target {
        job.session_target = target;
    }
    if let Some(delete_after_run) = patch.delete_after_run {
        job.delete_after_run = delete_after_run;
    }
    if let Some(agent_id) = patch.agent_id {
        job.agent_id = agent_id;
    }
    if let Some(policy) = patch.policy {
        job.policy = policy;
    }
    if let Some(origin) = patch.origin {
        job.origin = origin;
    }
    if schedule_changed {
        job.next_run = next_run_for_schedule(&job.schedule, Utc::now())?;
    } else if job.enabled && !was_enabled {
        // A job that sat disabled past its next run would fire the instant
        // it is enabled; move a stale next run to the next occurrence.
        let now = Utc::now();
        if job.next_run <= now {
            job.next_run = next_run_for_schedule(&job.schedule, now)?;
        }
    }
    Ok(job)
}
