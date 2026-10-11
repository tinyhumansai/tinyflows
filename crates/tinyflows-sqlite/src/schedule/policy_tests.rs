use super::*;
use rusqlite::Connection;

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
