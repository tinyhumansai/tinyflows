//! Row mapping, JSON decoding of stored schedules/delivery, and the SQLite
//! connection + schema-migration helpers shared by the cron store's job and
//! run-history modules.

use super::CronStoreOptions;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::Connection;
use tinyflows_schedule::{CronJob, DeliveryConfig, JobOrigin, JobType, Schedule, SessionTarget};

/// The `cron_jobs` columns every job SELECT reads, in the order
/// [`map_cron_job_row`] indexes them. New columns are appended, never inserted.
pub(super) const JOB_COLUMNS: &str =
    "id, expression, command, schedule, job_type, prompt, name, session_target, model,
     enabled, delivery, delete_after_run, created_at, next_run, last_run, last_status, last_output,
     agent_id, origin, retries, single_flight, claim_generation";

pub(super) fn map_cron_job_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CronJob> {
    let expression: String = row.get(1)?;
    let schedule_raw: Option<String> = row.get(3)?;
    let schedule =
        decode_schedule(schedule_raw.as_deref(), &expression).map_err(sql_conversion_error)?;

    let delivery_raw: Option<String> = row.get(10)?;
    let delivery = decode_delivery(delivery_raw.as_deref()).map_err(sql_conversion_error)?;

    let next_run_raw: String = row.get(13)?;
    let last_run_raw: Option<String> = row.get(14)?;
    let created_at_raw: String = row.get(12)?;
    let origin_raw: Option<String> = row.get(18)?;
    let origin = decode_origin(origin_raw.as_deref()).map_err(sql_conversion_error)?;

    Ok(CronJob {
        claim_generation: u64::try_from(row.get::<_, i64>(21)?)
            .map_err(|error| sql_conversion_error(error.into()))?,
        policy: tinyflows_schedule::JobPolicy {
            retries: row
                .get::<_, Option<i64>>(19)?
                .map(|n| u32::try_from(n.max(0)).unwrap_or(u32::MAX)),
            single_flight: row.get::<_, i64>(20)? != 0,
        },
        id: row.get(0)?,
        expression,
        schedule,
        command: row.get(2)?,
        job_type: JobType::parse(&row.get::<_, String>(4)?),
        prompt: row.get(5)?,
        name: row.get(6)?,
        session_target: SessionTarget::parse(&row.get::<_, String>(7)?),
        model: row.get(8)?,
        agent_id: row.get(17)?,
        enabled: row.get::<_, i64>(9)? != 0,
        delivery,
        delete_after_run: row.get::<_, i64>(11)? != 0,
        created_at: parse_rfc3339(&created_at_raw).map_err(sql_conversion_error)?,
        next_run: parse_rfc3339(&next_run_raw).map_err(sql_conversion_error)?,
        last_run: match last_run_raw {
            Some(raw) => Some(parse_rfc3339(&raw).map_err(sql_conversion_error)?),
            None => None,
        },
        last_status: row.get(15)?,
        last_output: row.get(16)?,
        origin,
    })
}

fn decode_schedule(schedule_raw: Option<&str>, expression: &str) -> Result<Schedule> {
    if let Some(raw) = schedule_raw {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return serde_json::from_str(trimmed)
                .with_context(|| format!("Failed to parse cron schedule JSON: {trimmed}"));
        }
    }

    if expression.trim().is_empty() {
        anyhow::bail!("Missing schedule and legacy expression for cron job")
    }

    Ok(Schedule::Cron {
        expr: expression.to_string(),
        tz: None,
        active_hours: None,
    })
}

fn decode_delivery(delivery_raw: Option<&str>) -> Result<DeliveryConfig> {
    if let Some(raw) = delivery_raw {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return serde_json::from_str(trimmed)
                .with_context(|| format!("Failed to parse cron delivery JSON: {trimmed}"));
        }
    }
    Ok(DeliveryConfig::default())
}

fn decode_origin(origin_raw: Option<&str>) -> Result<Option<JobOrigin>> {
    match origin_raw.map(str::trim) {
        Some(raw) if !raw.is_empty() => serde_json::from_str(raw)
            .map(Some)
            .with_context(|| format!("Failed to parse cron origin JSON: {raw}")),
        _ => Ok(None),
    }
}

/// Serializes a job origin for the `origin` column (`NULL` when absent).
pub(super) fn encode_origin(origin: Option<&JobOrigin>) -> Result<Option<String>> {
    origin
        .map(serde_json::to_string)
        .transpose()
        .context("Failed to serialize cron origin")
}

/// Adds `table.name` when the table lacks it. `table` is one of this module's
/// own table names, never caller input.
fn add_column_if_missing(conn: &Connection, table: &str, name: &str, sql_type: &str) -> Result<()> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let col_name: String = row.get(1)?;
        if col_name == name {
            return Ok(());
        }
    }
    // Drop the statement/rows before executing ALTER to release any locks
    drop(rows);
    drop(stmt);

    // Tolerate "duplicate column name" errors to handle the race where
    // another process adds the column between our PRAGMA check and ALTER.
    match conn.execute(
        &format!("ALTER TABLE {table} ADD COLUMN {name} {sql_type}"),
        [],
    ) {
        Ok(_) => Ok(()),
        Err(rusqlite::Error::SqliteFailure(err, Some(ref msg)))
            if msg.contains("duplicate column name") =>
        {
            tracing::debug!("Column {table}.{name} already exists (concurrent migration): {err}");
            Ok(())
        }
        Err(e) => Err(e).with_context(|| format!("Failed to add {table}.{name}")),
    }
}

/// Opens the cron store and runs `f` against it.
///
/// Creates the database's parent directory if needed, opens the SQLite file
/// through the driver's native mode (see `crate::native`), and creates or
/// migrates the schema idempotently (missing columns are added
/// and the flow-command index is created) before handing over the connection,
/// so a database written by an older build opens unchanged.
///
/// `f` runs while the driver's per-file lock is held, and that lock is not
/// reentrant: do the work on the `Connection` passed in. Calling another
/// store API for the same database from inside `f` (which opens the same
/// shared handle) would wait on the lock forever.
pub fn with_connection<T>(
    opts: &CronStoreOptions,
    f: impl FnOnce(&Connection) -> Result<T>,
) -> Result<T> {
    let db_path = &opts.db_path;
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create cron directory: {}", parent.display()))?;
    }

    crate::native::run(db_path, |conn| {
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
         CREATE TABLE IF NOT EXISTS cron_jobs (
            id               TEXT PRIMARY KEY,
            expression       TEXT NOT NULL,
            command          TEXT NOT NULL,
            schedule         TEXT,
            job_type         TEXT NOT NULL DEFAULT 'shell',
            prompt           TEXT,
            name             TEXT,
            session_target   TEXT NOT NULL DEFAULT 'isolated',
            model            TEXT,
            enabled          INTEGER NOT NULL DEFAULT 1,
            delivery         TEXT,
            delete_after_run INTEGER NOT NULL DEFAULT 0,
            created_at       TEXT NOT NULL,
            next_run         TEXT NOT NULL,
            last_run         TEXT,
            last_status      TEXT,
            last_output      TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_cron_jobs_next_run ON cron_jobs(next_run);

        CREATE TABLE IF NOT EXISTS cron_runs (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            job_id      TEXT NOT NULL,
            started_at  TEXT NOT NULL,
            finished_at TEXT NOT NULL,
            status      TEXT NOT NULL,
            output      TEXT,
            duration_ms INTEGER,
            FOREIGN KEY (job_id) REFERENCES cron_jobs(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_cron_runs_job_id ON cron_runs(job_id);
        CREATE INDEX IF NOT EXISTS idx_cron_runs_started_at ON cron_runs(started_at);
        CREATE INDEX IF NOT EXISTS idx_cron_runs_job_started ON cron_runs(job_id, started_at);",
        )
        .context("Failed to initialize cron schema")?;

        add_column_if_missing(conn, "cron_jobs", "schedule", "TEXT")?;
        add_column_if_missing(
            conn,
            "cron_jobs",
            "job_type",
            "TEXT NOT NULL DEFAULT 'shell'",
        )?;
        add_column_if_missing(conn, "cron_jobs", "prompt", "TEXT")?;
        add_column_if_missing(conn, "cron_jobs", "name", "TEXT")?;
        add_column_if_missing(
            conn,
            "cron_jobs",
            "session_target",
            "TEXT NOT NULL DEFAULT 'isolated'",
        )?;
        add_column_if_missing(conn, "cron_jobs", "model", "TEXT")?;
        add_column_if_missing(conn, "cron_jobs", "enabled", "INTEGER NOT NULL DEFAULT 1")?;
        add_column_if_missing(conn, "cron_jobs", "delivery", "TEXT")?;
        add_column_if_missing(
            conn,
            "cron_jobs",
            "delete_after_run",
            "INTEGER NOT NULL DEFAULT 0",
        )?;
        add_column_if_missing(conn, "cron_jobs", "agent_id", "TEXT")?;
        add_column_if_missing(conn, "cron_jobs", "origin", "TEXT")?;
        add_column_if_missing(conn, "cron_runs", "delivery_status", "TEXT")?;
        add_column_if_missing(conn, "cron_jobs", "retries", "INTEGER")?;
        add_column_if_missing(
            conn,
            "cron_jobs",
            "single_flight",
            "INTEGER NOT NULL DEFAULT 0",
        )?;
        add_column_if_missing(
            conn,
            "cron_jobs",
            "claim_generation",
            "INTEGER NOT NULL DEFAULT 0",
        )?;
        migrate_policies(conn)?;
        ensure_flow_command_index(conn)?;

        f(conn)
    })
}

/// Atomically import legacy host policies before dropping their separate table.
/// Defaults on new columns leave old seeded/system jobs unchanged.
fn migrate_policies(conn: &Connection) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    let exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='cron_job_policies')", [], |r| r.get(0))?;
    if exists {
        let malformed: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM cron_job_policies
             WHERE (retries IS NOT NULL AND
                    (typeof(retries) != 'integer' OR retries < 0 OR retries > 4294967295))
                OR typeof(single_flight) != 'integer' OR single_flight NOT IN (0, 1))",
            [],
            |row| row.get(0),
        )?;
        if malformed {
            anyhow::bail!("Invalid legacy cron policy; migration left the policy table unchanged");
        }
        tx.execute_batch("UPDATE cron_jobs SET
            retries = (SELECT retries FROM cron_job_policies p WHERE p.job_id=cron_jobs.id),
            single_flight = (SELECT single_flight != 0 FROM cron_job_policies p WHERE p.job_id=cron_jobs.id)
            WHERE EXISTS(SELECT 1 FROM cron_job_policies p WHERE p.job_id=cron_jobs.id);
            DROP TABLE cron_job_policies;")?;
    }
    tx.commit()?;
    Ok(())
}

/// Creates the `idx_cron_jobs_flow_command` partial unique index, first
/// reconciling any duplicate flow-schedule rows an older build could write.
///
/// The index guards against duplicate flow-schedule cron bindings under a
/// concurrent `bind_schedule_trigger`, which does check-then-act
/// (`find_flow_schedule_job` then `add_flow_schedule_job`): two racing binds
/// for one flow could otherwise each observe "no job" and insert a duplicate.
/// It is scoped to `job_type = 'flow'` so it never constrains shell/agent
/// jobs, which may legitimately share a `command`.
///
/// A database written before the index existed may already hold such
/// duplicates, and `CREATE UNIQUE INDEX` would then fail on every connection.
/// So when the index is missing, the earliest-created row per flow is kept,
/// the rest are deleted (their run history cascades), and the index is created,
/// all in one transaction. Once the index exists this is a single lookup.
fn ensure_flow_command_index(conn: &Connection) -> Result<()> {
    let exists: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master
                           WHERE type = 'index' AND name = 'idx_cron_jobs_flow_command')",
            [],
            |row| row.get(0),
        )
        .context("Failed to look up the flow-command index")?;
    if exists {
        return Ok(());
    }

    let tx = conn.unchecked_transaction()?;
    let removed = tx
        .execute(
            "DELETE FROM cron_jobs
             WHERE job_type = 'flow'
               AND EXISTS (
                 SELECT 1 FROM cron_jobs AS keep
                 WHERE keep.job_type = 'flow'
                   AND keep.command = cron_jobs.command
                   AND (keep.created_at < cron_jobs.created_at
                        OR (keep.created_at = cron_jobs.created_at
                            AND keep.rowid < cron_jobs.rowid))
               )",
            [],
        )
        .context("Failed to reconcile duplicate flow-schedule jobs")?;
    if removed > 0 {
        tracing::warn!(
            target: "cron",
            removed,
            "[cron] removed duplicate flow-schedule jobs before creating the flow-command index"
        );
    }
    tx.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_cron_jobs_flow_command
             ON cron_jobs(command) WHERE job_type = 'flow';",
    )
    .context("Failed to create the flow-command index")?;
    tx.commit()
        .context("Failed to commit the flow-command index migration")?;
    Ok(())
}

pub(super) fn parse_rfc3339(raw: &str) -> Result<DateTime<Utc>> {
    let parsed = DateTime::parse_from_rfc3339(raw)
        .with_context(|| format!("Invalid RFC3339 timestamp in cron DB: {raw}"))?;
    Ok(parsed.with_timezone(&Utc))
}

pub(super) fn sql_conversion_error(err: anyhow::Error) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(err.into())
}
