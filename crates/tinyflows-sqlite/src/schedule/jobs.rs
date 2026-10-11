//! Cron job CRUD: creating shell/agent/flow-schedule jobs, listing,
//! patching, deduplicating, and selecting due jobs.

use super::CronStoreOptions;
use super::agent_jobs::{AgentJobSpec, add_agent_job_from_spec};
use super::schema::{JOB_COLUMNS, encode_origin, map_cron_job_row, with_connection};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::params;
use tinyflows_schedule::{
    CronJob, CronJobPatch, DeliveryConfig, JobType, Schedule, SessionTarget, next_run_for_schedule,
    schedule_cron_expression, validate_agent_schedule, validate_schedule,
};
use uuid::Uuid;

/// Adds an enabled shell job on a plain cron `expression` (see [`add_shell_job`]).
pub fn add_job(opts: &CronStoreOptions, expression: &str, command: &str) -> Result<CronJob> {
    let schedule = Schedule::Cron {
        expr: expression.to_string(),
        tz: None,
        active_hours: None,
    };
    add_shell_job(opts, None, schedule, command)
}

/// Adds an enabled, optionally named shell job running `command` on `schedule`.
pub fn add_shell_job(
    opts: &CronStoreOptions,
    name: Option<String>,
    schedule: Schedule,
    command: &str,
) -> Result<CronJob> {
    let now = Utc::now();
    validate_schedule(&schedule, now)?;
    let next_run = next_run_for_schedule(&schedule, now)?;
    let id = Uuid::new_v4().to_string();
    let expression = schedule_cron_expression(&schedule).unwrap_or_default();
    let schedule_json = serde_json::to_string(&schedule)?;

    with_connection(opts, |conn| {
        conn.execute(
            "INSERT INTO cron_jobs (
                id, expression, command, schedule, job_type, prompt, name, session_target, model,
                enabled, delivery, delete_after_run, created_at, next_run
             ) VALUES (?1, ?2, ?3, ?4, 'shell', NULL, ?5, 'isolated', NULL, 1, ?6, 0, ?7, ?8)",
            params![
                id,
                expression,
                command,
                schedule_json,
                name,
                serde_json::to_string(&DeliveryConfig::default())?,
                now.to_rfc3339(),
                next_run.to_rfc3339(),
            ],
        )
        .context("Failed to insert cron shell job")?;
        Ok(())
    })?;

    get_job(opts, &id)
}

/// Adds an agent job with no agent definition (see [`add_agent_job_with_definition`]).
#[allow(clippy::too_many_arguments)]
pub fn add_agent_job(
    opts: &CronStoreOptions,
    name: Option<String>,
    schedule: Schedule,
    prompt: &str,
    session_target: SessionTarget,
    model: Option<String>,
    delivery: Option<DeliveryConfig>,
    delete_after_run: bool,
) -> Result<CronJob> {
    add_agent_job_with_definition(
        opts,
        name,
        schedule,
        prompt,
        session_target,
        model,
        delivery,
        delete_after_run,
        None,
        true,
    )
}

/// Like [`add_agent_job`] but accepts an optional built-in agent definition
/// ID. When set, the scheduler resolves the agent definition from the
/// registry and runs with its prompt, tool allowlist, and iteration cap.
/// Stores no origin; see [`add_agent_job_from_spec`] for that.
#[allow(clippy::too_many_arguments)]
pub fn add_agent_job_with_definition(
    opts: &CronStoreOptions,
    name: Option<String>,
    schedule: Schedule,
    prompt: &str,
    session_target: SessionTarget,
    model: Option<String>,
    delivery: Option<DeliveryConfig>,
    delete_after_run: bool,
    agent_id: Option<String>,
    enabled: bool,
) -> Result<CronJob> {
    add_agent_job_from_spec(
        opts,
        AgentJobSpec {
            name,
            schedule,
            prompt: prompt.to_string(),
            session_target,
            model,
            delivery,
            delete_after_run,
            agent_id,
            enabled,
            origin: None,
        },
    )
}

/// Registers the cron job that fires a `flows::Flow`'s `schedule` trigger
/// (issue B2). The flow's id is stored in `command` — a flow-schedule job has
/// no shell command / agent prompt of its own, it only needs to name which
/// flow to tick (see `JobType::Flow`'s doc). On fire the scheduler publishes
/// `DomainEvent::FlowScheduleTick { flow_id: command }` instead of running
/// anything; `flows::bus::FlowTriggerSubscriber` does the actual dispatch.
///
/// Race-safe / idempotent: `bind_schedule_trigger` does check-then-act
/// (`find_flow_schedule_job` then this function), so two concurrent binds for
/// the same flow can both observe "no job yet". The `idx_cron_jobs_flow_command`
/// partial unique index (flow jobs only) turns the loser's `INSERT` into a
/// no-op via `ON CONFLICT ... DO NOTHING`, and that loser then looks up and
/// returns the winner's row instead of erroring — callers always get back
/// exactly one cron job for `flow_id`, never a duplicate and never a
/// constraint-violation error.
pub fn add_flow_schedule_job(
    opts: &CronStoreOptions,
    flow_id: &str,
    schedule: Schedule,
) -> Result<CronJob> {
    let now = Utc::now();
    validate_schedule(&schedule, now)?;
    let next_run = next_run_for_schedule(&schedule, now)?;
    let id = Uuid::new_v4().to_string();
    let expression = schedule_cron_expression(&schedule).unwrap_or_default();
    let schedule_json = serde_json::to_string(&schedule)?;
    let name = format!("flow:{flow_id}");

    let inserted_rows = with_connection(opts, |conn| {
        let rows = conn
            .execute(
                "INSERT INTO cron_jobs (
                    id, expression, command, schedule, job_type, prompt, name, session_target, model,
                    enabled, delivery, delete_after_run, created_at, next_run
                 ) VALUES (?1, ?2, ?3, ?4, 'flow', NULL, ?5, 'isolated', NULL, 1, ?6, 0, ?7, ?8)
                 ON CONFLICT (command) WHERE job_type = 'flow' DO NOTHING",
                params![
                    id,
                    expression,
                    flow_id,
                    schedule_json,
                    name,
                    serde_json::to_string(&DeliveryConfig::default())?,
                    now.to_rfc3339(),
                    next_run.to_rfc3339(),
                ],
            )
            .context("Failed to insert cron flow-schedule job")?;
        Ok(rows)
    })?;

    if inserted_rows > 0 {
        get_job(opts, &id)
    } else {
        // Lost the race — another caller already holds the flow-schedule job
        // for this flow_id/command. Return its row rather than erroring so
        // `add_flow_schedule_job` is safe to call twice concurrently.
        tracing::debug!(
            target: "cron",
            %flow_id,
            "[cron] add_flow_schedule_job: insert conflicted with an existing flow job — returning the existing binding"
        );
        find_flow_schedule_job(opts, flow_id)?.with_context(|| {
            format!(
                "add_flow_schedule_job: insert for flow '{flow_id}' conflicted but no existing \
                 flow-schedule job was found"
            )
        })
    }
}

/// Finds the cron job (if any) registered for a flow's `schedule` trigger —
/// used by `flows::ops::flows_set_enabled` to make enable/disable idempotent
/// (re-use the existing binding rather than creating a duplicate) and to tear
/// it down on disable.
pub fn find_flow_schedule_job(opts: &CronStoreOptions, flow_id: &str) -> Result<Option<CronJob>> {
    with_connection(opts, |conn| {
        let mut stmt = conn.prepare(&format!(
            "SELECT {JOB_COLUMNS} FROM cron_jobs WHERE job_type = 'flow' AND command = ?1 LIMIT 1"
        ))?;
        let mut rows = stmt.query(params![flow_id])?;
        match rows.next()? {
            Some(row) => Ok(Some(map_cron_job_row(row)?)),
            None => Ok(None),
        }
    })
}

/// Returns every job, ordered by next run.
pub fn list_jobs(opts: &CronStoreOptions) -> Result<Vec<CronJob>> {
    with_connection(opts, |conn| {
        let mut stmt = conn.prepare(&format!(
            "SELECT {JOB_COLUMNS} FROM cron_jobs ORDER BY next_run ASC"
        ))?;

        let rows = stmt.query_map([], map_cron_job_row)?;

        let mut jobs = Vec::new();
        for row in rows {
            jobs.push(row?);
        }
        Ok(jobs)
    })
}

/// Returns the job with `job_id`, or an error when there is none.
pub fn get_job(opts: &CronStoreOptions, job_id: &str) -> Result<CronJob> {
    with_connection(opts, |conn| {
        let mut stmt = conn.prepare(&format!(
            "SELECT {JOB_COLUMNS} FROM cron_jobs WHERE id = ?1"
        ))?;

        let mut rows = stmt.query(params![job_id])?;
        if let Some(row) = rows.next()? {
            map_cron_job_row(row).map_err(Into::into)
        } else {
            anyhow::bail!("Cron job '{job_id}' not found")
        }
    })
}

/// Deletes the job with `id` and its run history; errors when there is none.
pub fn remove_job(opts: &CronStoreOptions, id: &str) -> Result<()> {
    let changed = with_connection(opts, |conn| {
        conn.execute("DELETE FROM cron_jobs WHERE id = ?1", params![id])
            .context("Failed to delete cron job")
    })?;

    if changed == 0 {
        anyhow::bail!("Cron job '{id}' not found");
    }

    Ok(())
}

/// Deletes every cron job in the workspace. Returns the number of rows removed.
///
/// Intended for the `openhuman.test_reset` RPC used by E2E specs to wipe state
/// between tests without restarting the sidecar. The cron scheduler picks up
/// the empty table on its next tick — no in-memory cache to invalidate.
pub fn clear_all_jobs(opts: &CronStoreOptions) -> Result<usize> {
    let removed = with_connection(opts, |conn| {
        conn.execute("DELETE FROM cron_jobs", params![])
            .context("Failed to clear cron jobs")
    })?;
    tracing::info!("[cron] cleared all cron jobs (removed {removed} rows)");
    Ok(removed)
}

/// Remove duplicate cron jobs that share the same `name`.
///
/// Older builds used a non-atomic check-then-insert in `seed_proactive_agents`,
/// which allowed two identical rows (e.g. two `morning_briefing` entries) to
/// land in the database when the function raced or was called twice before the
/// first insert committed. The `cron_jobs` table has no `UNIQUE` constraint on
/// `name`, so both rows persist and the Routines screen renders two cards.
///
/// For each duplicated name this function keeps the row with the most
/// `cron_runs` history (ties broken by earliest `created_at`) and deletes
/// all others. Returns the total number of rows removed across all names.
///
/// Idempotent: calling it on a database with no duplicates removes nothing
/// and returns `Ok(0)`.
pub fn dedup_named_jobs(opts: &CronStoreOptions) -> Result<usize> {
    with_connection(opts, |conn| {
        // 1. Find all names that appear more than once.
        let duplicated_names: Vec<String> = {
            let mut stmt = conn.prepare(
                "SELECT name FROM cron_jobs \
                 WHERE name IS NOT NULL \
                 GROUP BY name \
                 HAVING COUNT(*) > 1",
            )?;
            let names = stmt.query_map([], |row| row.get::<_, String>(0))?;
            let mut out = Vec::new();
            for n in names {
                out.push(n?);
            }
            out
        };

        if duplicated_names.is_empty() {
            return Ok(0);
        }

        let mut canonical_stmt = conn.prepare(
            "SELECT j.id \
             FROM cron_jobs j \
             LEFT JOIN cron_runs r ON r.job_id = j.id \
             WHERE j.name = ?1 \
             GROUP BY j.id \
             ORDER BY COUNT(r.id) DESC, j.created_at ASC, j.id ASC \
             LIMIT 1",
        )?;

        let mut total_removed = 0usize;
        for name in &duplicated_names {
            // 2. Find the canonical id: most run history, tie-break earliest created_at.
            let canonical_id: Option<String> = {
                let mut rows = canonical_stmt.query(params![name])?;
                rows.next()?.map(|r| r.get::<_, String>(0)).transpose()?
            };

            let Some(keep_id) = canonical_id else {
                continue;
            };

            // 3. Delete every other row sharing this name.
            let deleted = conn.execute(
                "DELETE FROM cron_jobs WHERE name = ?1 AND id != ?2",
                params![name, keep_id],
            )?;
            tracing::info!(
                "[cron] dedup_named_jobs: removed {deleted} duplicate(s) of '{name}' \
                 (keeping id={keep_id})"
            );
            total_removed += deleted;
        }

        Ok(total_removed)
    })
}

/// Returns enabled jobs due at `now`, earliest first, at most `max_tasks`.
pub fn due_jobs(opts: &CronStoreOptions, now: DateTime<Utc>) -> Result<Vec<CronJob>> {
    due_jobs_excluding(opts, now, &[])
}

/// Due jobs excluding examined identities before the configured batch limit.
pub fn due_jobs_excluding(
    opts: &CronStoreOptions,
    now: DateTime<Utc>,
    excluded: &[String],
) -> Result<Vec<CronJob>> {
    let lim = i64::try_from(opts.max_tasks.max(1)).context("Scheduler max_tasks overflows i64")?;
    let excluded = serde_json::to_string(excluded)?;
    with_connection(opts, |conn| {
        let mut stmt = conn.prepare(&format!(
            "SELECT {JOB_COLUMNS} FROM cron_jobs
                 WHERE enabled = 1 AND next_run <= ?1
                   AND id NOT IN (SELECT value FROM json_each(?2))
                 ORDER BY next_run ASC
                 LIMIT ?3"
        ))?;
        let rows = stmt.query_map(params![now.to_rfc3339(), excluded, lim], map_cron_job_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    })
}

/// Applies `patch` to the job with `job_id` and returns the updated row.
pub fn update_job(opts: &CronStoreOptions, job_id: &str, patch: CronJobPatch) -> Result<CronJob> {
    let mut job = get_job(opts, job_id)?;
    tinyflows_schedule::check_patch(&job, &patch)?;
    let was_enabled = job.enabled;
    let mut schedule_changed = false;

    if let Some(schedule) = patch.schedule {
        // The agent-only floor applies whenever the schedule is (re)set, so a
        // row that predates it keeps running untouched until its schedule is
        // edited — and then has to comply like a new job.
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
        // Disabled→enabled transition (e.g. opting into a seeded morning
        // briefing). A job that sat disabled past its originally computed
        // next_run would otherwise fire immediately on opt-in, because the
        // scheduler selects `enabled = 1 AND next_run <= now`. Refresh a stale
        // next_run so the first run lands on the next scheduled occurrence
        // rather than firing the instant the user flips the switch.
        let now = Utc::now();
        if job.next_run <= now {
            let refreshed = next_run_for_schedule(&job.schedule, now)?;
            tracing::debug!(
                job_id = %job.id,
                stale_next_run = %job.next_run.to_rfc3339(),
                next_run = %refreshed.to_rfc3339(),
                "[cron::update_job] refreshed stale next_run on disabled→enabled transition"
            );
            job.next_run = refreshed;
        }
    }

    with_connection(opts, |conn| {
        conn.execute(
            "UPDATE cron_jobs
             SET expression = ?1, command = ?2, schedule = ?3, job_type = ?4, prompt = ?5, name = ?6,
                 session_target = ?7, model = ?8, enabled = ?9, delivery = ?10, delete_after_run = ?11,
                 next_run = ?12, agent_id = ?14, origin = ?15, retries = ?16, single_flight = ?17
             WHERE id = ?13",
            params![
                job.expression,
                job.command,
                serde_json::to_string(&job.schedule)?,
                job.job_type.as_str(),
                job.prompt,
                job.name,
                job.session_target.as_str(),
                job.model,
                if job.enabled { 1 } else { 0 },
                serde_json::to_string(&job.delivery)?,
                if job.delete_after_run { 1 } else { 0 },
                job.next_run.to_rfc3339(),
                job.id,
                job.agent_id,
                encode_origin(job.origin.as_ref())?,
                job.policy.retries.map(i64::from),
                i64::from(job.policy.single_flight),
            ],
        )
        .context("Failed to update cron job")?;
        Ok(())
    })?;

    get_job(opts, job_id)
}
