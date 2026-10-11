//! Existing user databases must open unchanged: these fixtures carry the
//! literal statements an older build wrote, independent of the ones the store
//! now issues.

use super::*;
use rusqlite::Connection;
use tempfile::TempDir;

/// An older `cron_jobs` / `cron_runs` layout: it has `job_type` but predates
/// schedule, prompt, name, session_target, model, enabled, delivery,
/// delete_after_run and agent_id.
const LEGACY_LAYOUT: &str = "
    CREATE TABLE cron_jobs (
        id          TEXT PRIMARY KEY,
        expression  TEXT NOT NULL,
        command     TEXT NOT NULL,
        job_type    TEXT NOT NULL DEFAULT 'shell',
        created_at  TEXT NOT NULL,
        next_run    TEXT NOT NULL,
        last_run    TEXT,
        last_status TEXT,
        last_output TEXT
    );
    CREATE INDEX idx_cron_jobs_next_run ON cron_jobs(next_run);
    CREATE TABLE cron_runs (
        id          INTEGER PRIMARY KEY AUTOINCREMENT,
        job_id      TEXT NOT NULL,
        started_at  TEXT NOT NULL,
        finished_at TEXT NOT NULL,
        status      TEXT NOT NULL,
        output      TEXT,
        duration_ms INTEGER,
        FOREIGN KEY (job_id) REFERENCES cron_jobs(id) ON DELETE CASCADE
    );";

fn columns(conn: &Connection) -> Vec<String> {
    let mut stmt = conn.prepare("PRAGMA table_info(cron_jobs)").unwrap();
    let rows = stmt.query_map([], |r| r.get::<_, String>(1)).unwrap();
    rows.map(Result::unwrap).collect()
}

#[test]
fn legacy_database_is_migrated_in_place_and_keeps_its_rows() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("cron").join("jobs.db");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(LEGACY_LAYOUT).unwrap();
        conn.execute(
            "INSERT INTO cron_jobs (id, expression, command, created_at, next_run)
             VALUES ('old', '*/5 * * * *', 'echo old', '2024-01-01T00:00:00+00:00', '2024-01-01T00:05:00+00:00')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO cron_runs (job_id, started_at, finished_at, status, output, duration_ms)
             VALUES ('old', '2024-01-01T00:00:00+00:00', '2024-01-01T00:00:01+00:00', 'ok', 'hi', 1000)",
            [],
        )
        .unwrap();
    }

    let opts = CronStoreOptions::new(&path);
    let job = get_job(&opts, "old").unwrap();
    assert_eq!(job.command, "echo old");
    assert_eq!(job.expression, "*/5 * * * *");
    assert!(job.enabled);
    assert_eq!(job.job_type, tinyflows_schedule::JobType::Shell);
    assert_eq!(
        job.session_target,
        tinyflows_schedule::SessionTarget::Isolated
    );
    assert_eq!(job.agent_id, None);
    assert_eq!(list_runs(&opts, "old", 10).unwrap().len(), 1);

    with_connection(&opts, |conn| {
        let cols = columns(conn);
        for expected in [
            "schedule",
            "job_type",
            "prompt",
            "name",
            "session_target",
            "model",
            "enabled",
            "delivery",
            "delete_after_run",
            "agent_id",
        ] {
            assert!(
                cols.iter().any(|c| c == expected),
                "missing {expected}: {cols:?}"
            );
        }
        Ok(())
    })
    .unwrap();
}

#[test]
fn schema_created_on_a_fresh_database_matches_the_pinned_layout() {
    let tmp = TempDir::new().unwrap();
    let opts = CronStoreOptions::new(tmp.path().join("nested").join("jobs.db"));
    with_connection(&opts, |conn| {
        let mut cols = columns(conn);
        cols.sort();
        let mut expected: Vec<String> = [
            "id",
            "expression",
            "command",
            "schedule",
            "job_type",
            "prompt",
            "name",
            "session_target",
            "model",
            "enabled",
            "delivery",
            "delete_after_run",
            "created_at",
            "next_run",
            "last_run",
            "last_status",
            "last_output",
            "agent_id",
            "origin",
            "retries",
            "single_flight",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        expected.sort();
        assert_eq!(cols, expected);

        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'index' AND name LIKE 'idx_%' ORDER BY name")
            .unwrap();
        let idx: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            idx,
            [
                "idx_cron_jobs_flow_command",
                "idx_cron_jobs_next_run",
                "idx_cron_runs_job_id",
                "idx_cron_runs_job_started",
                "idx_cron_runs_started_at",
            ]
        );
        Ok(())
    })
    .unwrap();
}

#[test]
fn a_row_written_with_the_full_current_columns_reads_back_verbatim() {
    let tmp = TempDir::new().unwrap();
    let opts = CronStoreOptions::new(tmp.path().join("jobs.db"));
    with_connection(&opts, |conn| {
        conn.execute(
            "INSERT INTO cron_jobs (id, expression, command, schedule, job_type, prompt, name,
                session_target, model, enabled, delivery, delete_after_run, created_at, next_run,
                last_run, last_status, last_output, agent_id)
             VALUES ('j1', '0 9 * * *', '', '{\"kind\":\"cron\",\"expr\":\"0 9 * * *\",\"tz\":\"UTC\"}',
                'agent', 'hello', 'brief', 'main', 'm1', 0, '{\"mode\":\"none\"}', 1,
                '2025-01-01T00:00:00+00:00', '2025-01-02T09:00:00+00:00',
                '2025-01-01T09:00:00+00:00', 'ok', 'done', 'agent-x')",
            [],
        )?;
        Ok(())
    })
    .unwrap();
    let job = get_job(&opts, "j1").unwrap();
    assert_eq!(job.name.as_deref(), Some("brief"));
    assert_eq!(job.prompt.as_deref(), Some("hello"));
    assert_eq!(job.agent_id.as_deref(), Some("agent-x"));
    assert_eq!(job.model.as_deref(), Some("m1"));
    assert!(!job.enabled);
    assert!(job.delete_after_run);
    assert_eq!(job.last_status.as_deref(), Some("ok"));
    assert_eq!(job.last_output.as_deref(), Some("done"));
}
