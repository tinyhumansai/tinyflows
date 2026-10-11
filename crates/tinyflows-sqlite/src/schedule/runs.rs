//! Cron run-history persistence: recording job outcomes with bounded output,
//! pruning old history, and reading it back.

use super::CronStoreOptions;
use super::schema::{parse_rfc3339, sql_conversion_error, with_connection};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::params;
use tinyflows_schedule::{CronJob, CronRun, DeliveryStatus, Schedule, next_run_for_schedule};

use tinyflows_schedule::truncate_cron_output;
pub use tinyflows_schedule::{MAX_CRON_OUTPUT_BYTES, TRUNCATED_OUTPUT_MARKER};

struct HistoryWrite<'a> {
    job_id: &'a str,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    status: &'a str,
    output: Option<&'a str>,
    duration_ms: i64,
    delivery_status: Option<DeliveryStatus>,
    expected_creation: Option<DateTime<Utc>>,
}

/// Records a run's outcome on the job row (`last_run`, `last_status`,
/// bounded `last_output`) without touching `next_run` or `enabled`.
pub fn record_last_run(
    opts: &CronStoreOptions,
    job_id: &str,
    finished_at: DateTime<Utc>,
    success: bool,
    output: &str,
) -> Result<()> {
    let status = if success { "ok" } else { "error" };
    let bounded_output = truncate_cron_output(output);
    with_connection(opts, |conn| {
        conn.execute(
            "UPDATE cron_jobs
             SET last_run = ?1, last_status = ?2, last_output = ?3
             WHERE id = ?4",
            params![finished_at.to_rfc3339(), status, bounded_output, job_id],
        )
        .context("Failed to update cron last run fields")?;
        Ok(())
    })
}

/// Records a run's outcome on the job row and advances `next_run` from now.
///
/// A `Schedule::At` job has no later occurrence: its next run is the same past
/// instant, which would leave it due on every poll. Such a job is disabled
/// here instead, so it runs once (the row and its history are kept).
pub fn reschedule_after_run(
    opts: &CronStoreOptions,
    job: &CronJob,
    success: bool,
    output: &str,
) -> Result<()> {
    let now = Utc::now();
    let next_run = next_run_for_schedule(&job.schedule, now)?;
    let one_shot = matches!(job.schedule, Schedule::At { .. });
    let status = if success { "ok" } else { "error" };
    let bounded_output = truncate_cron_output(output);

    with_connection(opts, |conn| {
        conn.execute(
            "UPDATE cron_jobs
             SET next_run = ?1, last_run = ?2, last_status = ?3, last_output = ?4,
                 enabled = CASE WHEN ?6 THEN 0 ELSE enabled END
             WHERE id = ?5",
            params![
                next_run.to_rfc3339(),
                now.to_rfc3339(),
                status,
                bounded_output,
                job.id,
                one_shot
            ],
        )
        .context("Failed to update cron job run state")?;
        Ok(())
    })
}

/// Appends one run to the job's history (bounded output) and prunes the
/// history to the newest [`CronStoreOptions::max_run_history`] runs, in one
/// transaction. Records no delivery status; see [`record_run_with_delivery`].
pub fn record_run(
    opts: &CronStoreOptions,
    job_id: &str,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    status: &str,
    output: Option<&str>,
    duration_ms: i64,
) -> Result<()> {
    record_run_with_delivery(
        opts,
        job_id,
        started_at,
        finished_at,
        status,
        output,
        duration_ms,
        None,
    )
}

/// [`record_run`] that also stores what happened to the run's result
/// (`cron_runs.delivery_status`, `NULL` when `None`).
#[allow(clippy::too_many_arguments)]
pub fn record_run_with_delivery(
    opts: &CronStoreOptions,
    job_id: &str,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    status: &str,
    output: Option<&str>,
    duration_ms: i64,
    delivery_status: Option<DeliveryStatus>,
) -> Result<()> {
    append_history(
        opts,
        HistoryWrite {
            job_id,
            started_at,
            finished_at,
            status,
            output,
            duration_ms,
            delivery_status,
            expected_creation: None,
        },
    )
}

/// Append a scheduler outcome only to the dispatch-time job creation.
/// A removed or re-created job is rejected without writing its history.
pub fn record_claimed_run(
    opts: &CronStoreOptions,
    record: &tinyflows_schedule::runtime::RunRecord,
) -> Result<()> {
    append_history(
        opts,
        HistoryWrite {
            job_id: &record.job_id,
            started_at: record.started_at,
            finished_at: record.finished_at,
            status: &record.status,
            output: Some(&record.output),
            duration_ms: (record.finished_at - record.started_at).num_milliseconds(),
            delivery_status: record.delivery_status.clone(),
            expected_creation: Some(record.job_created_at),
        },
    )
}

fn append_history(opts: &CronStoreOptions, write: HistoryWrite<'_>) -> Result<()> {
    let HistoryWrite {
        job_id,
        started_at,
        finished_at,
        status,
        output,
        duration_ms,
        delivery_status,
        expected_creation,
    } = write;
    tracing::debug!(
        target: "cron",
        %job_id,
        status,
        delivery_status = delivery_status.as_ref().map(DeliveryStatus::as_str),
        "[cron] record_run_with_delivery"
    );
    let bounded_output = output.map(truncate_cron_output);
    with_connection(opts, |conn| {
        // Wrap INSERT + pruning DELETE in an explicit transaction so that
        // if the DELETE fails, the INSERT is rolled back and the run table
        // cannot grow unboundedly.
        let tx = conn.unchecked_transaction()?;
        if let Some(expected) = expected_creation {
            let created: String = tx.query_row(
                "SELECT created_at FROM cron_jobs WHERE id=?1",
                [job_id],
                |row| row.get(0),
            )?;
            anyhow::ensure!(
                DateTime::parse_from_rfc3339(&created)?.with_timezone(&Utc) == expected,
                "cron run belongs to a removed job creation"
            );
        }

        tx.execute(
            "INSERT INTO cron_runs (job_id, started_at, finished_at, status, output, duration_ms,
                 delivery_status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                job_id,
                started_at.to_rfc3339(),
                finished_at.to_rfc3339(),
                status,
                bounded_output.as_deref(),
                duration_ms,
                delivery_status.as_ref().map(DeliveryStatus::as_str),
            ],
        )
        .context("Failed to insert cron run")?;

        let keep = opts.max_run_history.max(1) as i64;
        tx.execute(
            "DELETE FROM cron_runs
             WHERE job_id = ?1
               AND id NOT IN (
                 SELECT id FROM cron_runs
                 WHERE job_id = ?1
                 ORDER BY started_at DESC, id DESC
                 LIMIT ?2
               )",
            params![job_id, keep],
        )
        .context("Failed to prune cron run history")?;

        tx.commit()
            .context("Failed to commit cron run transaction")?;
        Ok(())
    })
}

/// Remove all "queued" placeholder records for a given job so that only the
/// real (ok/error) result row remains in the run history.
pub fn delete_queued_runs(opts: &CronStoreOptions, job_id: &str) -> Result<usize> {
    with_connection(opts, |conn| {
        let deleted = conn.execute(
            "DELETE FROM cron_runs WHERE job_id = ?1 AND status = 'queued'",
            params![job_id],
        )?;
        Ok(deleted)
    })
}

/// Reads `cron_runs.delivery_status`; a value this build does not know reads
/// as `None` rather than failing the whole history listing.
fn decode_delivery_status(raw: Option<String>) -> Option<DeliveryStatus> {
    let raw = raw?;
    let parsed = DeliveryStatus::parse(&raw);
    if parsed.is_none() {
        tracing::debug!(target: "cron", raw, "[cron] list_runs: unknown delivery_status, reading as none");
    }
    parsed
}

/// Returns the job's newest runs, most recent first, at most `limit` (min 1).
pub fn list_runs(opts: &CronStoreOptions, job_id: &str, limit: usize) -> Result<Vec<CronRun>> {
    with_connection(opts, |conn| {
        let lim = i64::try_from(limit.max(1)).context("Run history limit overflow")?;
        let mut stmt = conn.prepare(
            "SELECT id, job_id, started_at, finished_at, status, output, duration_ms,
                    delivery_status
             FROM cron_runs
             WHERE job_id = ?1
             ORDER BY started_at DESC, id DESC
             LIMIT ?2",
        )?;

        let rows = stmt.query_map(params![job_id, lim], |row| {
            Ok(CronRun {
                id: row.get(0)?,
                job_id: row.get(1)?,
                started_at: parse_rfc3339(&row.get::<_, String>(2)?)
                    .map_err(sql_conversion_error)?,
                finished_at: parse_rfc3339(&row.get::<_, String>(3)?)
                    .map_err(sql_conversion_error)?,
                status: row.get(4)?,
                output: row.get(5)?,
                duration_ms: row.get(6)?,
                delivery_status: decode_delivery_status(row.get(7)?),
            })
        })?;

        let mut runs = Vec::new();
        for row in rows {
            runs.push(row?);
        }
        Ok(runs)
    })
}
