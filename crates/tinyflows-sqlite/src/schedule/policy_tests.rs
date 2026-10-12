use super::*;
use rusqlite::Connection;

#[test]
fn malformed_legacy_retry_budgets_fail_closed_without_mutating_seeded_jobs() {
    for retries in [-1_i64, i64::from(u32::MAX) + 1] {
        let dir = tempfile::tempdir().unwrap();
        let opts = CronStoreOptions::new(dir.path().join("jobs.db"));
        let job = add_agent_job_with_definition(
            &opts,
            Some("system".into()),
            tinyflows_schedule::Schedule::Every { every_ms: 300_000 },
            "seeded prompt",
            tinyflows_schedule::SessionTarget::Isolated,
            None,
            None,
            false,
            Some("welcome".into()),
            true,
        )
        .unwrap();
        let conn = Connection::open(&opts.db_path).unwrap();
        conn.execute_batch("CREATE TABLE cron_job_policies(job_id TEXT PRIMARY KEY, retries INTEGER, single_flight INTEGER NOT NULL DEFAULT 0);").unwrap();
        conn.execute(
            "INSERT INTO cron_job_policies VALUES (?1, ?2, 1)",
            rusqlite::params![&job.id, retries],
        )
        .unwrap();

        assert!(
            get_job(&opts, &job.id).is_err(),
            "invalid retry budget {retries} must prevent scheduling"
        );
        let preserved: i64 = conn
            .query_row(
                "SELECT retries FROM cron_job_policies WHERE job_id=?1",
                [&job.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(preserved, retries);
        let unchanged: (Option<i64>, i64, String) = conn
            .query_row(
                "SELECT retries, single_flight, prompt FROM cron_jobs WHERE id=?1",
                [&job.id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(unchanged, (None, 0, "seeded prompt".into()));
    }
}

#[test]
fn legacy_policy_rows_migrate_without_changing_seeded_jobs() {
    let dir = tempfile::tempdir().unwrap();
    let opts = CronStoreOptions::new(dir.path().join("jobs.db"));
    let job = add_agent_job_with_definition(
        &opts,
        Some("system".into()),
        tinyflows_schedule::Schedule::Every { every_ms: 300_000 },
        "seeded prompt",
        tinyflows_schedule::SessionTarget::Isolated,
        None,
        None,
        false,
        Some("welcome".into()),
        true,
    )
    .unwrap();
    let before = serde_json::to_value(&job).unwrap();
    let conn = Connection::open(&opts.db_path).unwrap();
    conn.execute_batch("CREATE TABLE cron_job_policies(job_id TEXT PRIMARY KEY, retries INTEGER, single_flight INTEGER NOT NULL DEFAULT 0);").unwrap();
    conn.execute("INSERT INTO cron_job_policies VALUES (?1, 0, 1)", [&job.id])
        .unwrap();
    drop(conn);
    let upgraded = get_job(&opts, &job.id).unwrap();
    let mut after = serde_json::to_value(upgraded).unwrap();
    let policy = after
        .as_object_mut()
        .unwrap()
        .remove("policy")
        .unwrap_or_default();
    assert_eq!(
        policy,
        serde_json::json!({"retries": 0, "single_flight": true})
    );
    assert_eq!(after, before);
    assert_eq!(list_jobs(&opts).unwrap().len(), 1);
    with_connection(&opts, |conn| {
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='cron_job_policies')",
            [],
            |r| r.get(0),
        )?;
        assert!(!exists);
        Ok(())
    })
    .unwrap();
}
