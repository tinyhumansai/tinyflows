//! Apply scheduler-computed mutations without replacing concurrent job edits.
use super::{
    CronStoreOptions,
    schema::{JOB_COLUMNS, map_cron_job_row, with_connection},
};
use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, params};
use tinyflows_schedule::{CronJob, Schedule, runtime::Completion, truncate_cron_output};

/// Advance only the original occurrence and definition incarnation.
/// One-shots are disabled before execution, so a crash cannot repeat them.
/// Returns false when it was removed, disabled, edited or already advanced.
pub fn claim_slot(options: &CronStoreOptions, job: &CronJob, next: DateTime<Utc>) -> Result<bool> {
    with_connection(options, |connection| {
        let tx = connection.unchecked_transaction()?;
        let current = tx
            .query_row(
                &format!("SELECT {JOB_COLUMNS} FROM cron_jobs WHERE id=?1"),
                [&job.id],
                map_cron_job_row,
            )
            .optional()?;
        let Some(current) = current else {
            return Ok(false);
        };
        if !current.enabled
            || current.created_at != job.created_at
            || !current.same_execution_definition(job)
            || current.schedule != job.schedule
            || current.next_run != job.next_run
        {
            return Ok(false);
        }
        tx.execute(
            "UPDATE cron_jobs SET next_run=?1, enabled=?2 WHERE id=?3",
            params![
                next.to_rfc3339(),
                !matches!(job.schedule, Schedule::At { .. }),
                job.id
            ],
        )?;
        tx.commit()?;
        Ok(true)
    })
}

/// Apply a computed completion. An edited schedule retains its next occurrence
/// and enabled state; the outcome still updates the original incarnation.
/// Call after claim_slot: enabled one-shots now mean an explicit re-enable.
pub fn complete_run(
    options: &CronStoreOptions,
    job: &CronJob,
    completion: &Completion,
) -> Result<()> {
    with_connection(options, |connection| {
        let tx = connection.unchecked_transaction()?;
        let current = tx
            .query_row(
                &format!("SELECT {JOB_COLUMNS} FROM cron_jobs WHERE id=?1"),
                [&job.id],
                map_cron_job_row,
            )
            .optional()?;
        let Some(current) = current else {
            return Ok(());
        };
        if current.created_at != job.created_at {
            return Ok(());
        }
        let same = current.schedule == job.schedule;
        if completion.remove
            && !current.enabled
            && current.delete_after_run
            && same
            && current.command == job.command
            && current.prompt == job.prompt
        {
            tx.execute("DELETE FROM cron_jobs WHERE id=?1", [&job.id])?;
        } else {
            let next = completion
                .next_run
                .filter(|next| same && *next >= current.next_run)
                .unwrap_or(current.next_run);
            // A one-shot was disabled by claim_slot. A currently enabled row
            // is an operator re-enable and must survive this old completion.
            let enabled = current.enabled
                && !(same && completion.disable && !matches!(job.schedule, Schedule::At { .. }));
            tx.execute("UPDATE cron_jobs SET next_run=?1, enabled=?2, last_run=?3, last_status=?4, last_output=?5 WHERE id=?6", params![next.to_rfc3339(), enabled, completion.finished_at.to_rfc3339(), if completion.success {"ok"} else {"error"}, truncate_cron_output(&completion.output), job.id])?;
        }
        tx.commit()?;
        Ok(())
    })
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
