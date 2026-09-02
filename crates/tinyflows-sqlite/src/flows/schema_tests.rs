//! Tests for the one-time schema DDL + migration gating.

use super::*;
use crate::flows::test_support::*;
use tempfile::TempDir;

#[test]
fn schema_initializes_correctly_on_a_fresh_database_and_is_idempotent_across_calls() {
    let tmp = TempDir::new().unwrap();
    let dir = test_dir(&tmp);

    // First-ever call against this database file in the process: exercises
    // the full schema DDL (CREATE TABLE batch + indexes) plus the
    // `require_approval` `add_column_if_missing` migration on a database that
    // has never been opened before.
    let flow = create_flow(
        &dir,
        "fresh-db".to_string(),
        trigger_graph(),
        true, // require_approval
        true,
    )
    .unwrap();
    assert!(
        flow.require_approval,
        "the post-hoc require_approval column must exist and be writable on a brand-new db"
    );

    // Repeat calls against the SAME path must not need (or re-run) DDL —
    // proves the cached "already initialized" state doesn't break ordinary
    // reads/writes on reuse.
    let (listed, skipped) = list_flows(&dir).unwrap();
    assert_eq!(skipped, 0);
    assert_eq!(listed.len(), 1);
    assert!(listed[0].require_approval);

    let reloaded = get_flow(&dir, &flow.id).unwrap().unwrap();
    assert!(reloaded.require_approval);

    let run_id = "run-schema-check";
    insert_flow_run(&dir, run_id, &flow.id, run_id, "2026-01-01T00:00:00Z").unwrap();
    assert!(get_flow_run(&dir, run_id).unwrap().is_some());
}

#[test]
fn schema_initializes_independently_for_each_distinct_database_path() {
    // Regression guard for the once-per-process cache: if it were keyed by a
    // single process-wide flag instead of by database path, opening a SECOND
    // independent workspace after the first would silently skip schema
    // creation and every write against it would fail with "no such table".
    let tmp_a = TempDir::new().unwrap();
    let dir_a = test_dir(&tmp_a);
    let flow_a = create_flow(&dir_a, "a".to_string(), trigger_graph(), false, true).unwrap();

    let tmp_b = TempDir::new().unwrap();
    let dir_b = test_dir(&tmp_b);
    let flow_b = create_flow(&dir_b, "b".to_string(), trigger_graph(), false, true).unwrap();

    assert_eq!(list_flows(&dir_a).unwrap().0.len(), 1);
    assert_eq!(list_flows(&dir_b).unwrap().0.len(), 1);
    assert_eq!(get_flow(&dir_a, &flow_a.id).unwrap().unwrap().id, flow_a.id);
    assert_eq!(get_flow(&dir_b, &flow_b.id).unwrap().unwrap().id, flow_b.id);
}

/// R-m8 regression: gating the DDL behind a per-path "already initialized" set
/// must not cost the store its self-healing.
///
/// Before the gate existed, the DDL ran on every `with_connection` call, so a
/// database deleted or replaced at runtime (workspace reset, manual deletion,
/// a restore) recovered on the very next call — `Connection::open` creates a
/// fresh empty file and `CREATE TABLE IF NOT EXISTS` repopulates it. With a
/// naive cache the set still reports "initialized" while the file behind it is
/// empty, and every query afterwards fails `no such table: flow_definitions`
/// until the process restarts. This pins the verify-on-hit that restores it.
#[test]
fn schema_reinitializes_when_the_database_file_is_deleted_at_runtime() {
    let tmp = TempDir::new().unwrap();
    let dir = test_dir(&tmp);

    // First use populates the per-path cache and creates the schema.
    let flow = create_flow(
        &dir,
        "before-deletion".to_string(),
        trigger_graph(),
        false,
        true,
    )
    .unwrap();
    let (flows, _skipped) = list_flows(&dir).unwrap();
    assert_eq!(flows.len(), 1, "sanity: the flow was persisted");

    // Simulate a workspace reset / manual deletion while the process lives on.
    let db_path = dir.join("flows.db");
    assert!(
        db_path.exists(),
        "sanity: the flows db exists before deletion"
    );
    std::fs::remove_file(&db_path).unwrap();
    // WAL sidecars must go too, or SQLite can resurrect pages from them.
    let _ = std::fs::remove_file(db_path.with_extension("db-wal"));
    let _ = std::fs::remove_file(db_path.with_extension("db-shm"));

    // The cache still says this path is initialized. Without the verify-on-hit
    // this errors with `no such table: flow_definitions`.
    let (flows_after, skipped_after) = list_flows(&dir)
        .expect("a deleted database must be re-initialized, not left wedged at 'no such table'");
    assert!(
        flows_after.is_empty(),
        "the recreated database starts empty — the prior flow is genuinely gone"
    );
    assert_eq!(skipped_after, 0, "an empty database skips nothing");

    // And the store is fully usable again, not merely readable.
    let recreated = create_flow(
        &dir,
        "after-deletion".to_string(),
        trigger_graph(),
        false,
        true,
    )
    .expect("writes must work against the re-initialized schema");
    assert_ne!(recreated.id, flow.id);
    let (flows_final, _) = list_flows(&dir).unwrap();
    assert_eq!(flows_final.len(), 1);
}

/// Companion to the deletion test: a database *replaced* at runtime with an
/// older/partial schema (rather than deleted) must also be re-migrated, not
/// trusted. This is the case a single-table `sqlite_master` presence probe
/// could not catch — `flow_definitions` is still there, so the probe would
/// honour the cache and the next read of a migrated column would fail with
/// `no such column`. The `PRAGMA user_version` gate detects the drift.
#[test]
fn older_on_disk_schema_under_a_cached_path_is_remigrated() {
    let tmp = TempDir::new().unwrap();
    let dir = test_dir(&tmp);

    // First use creates the full (versioned) schema and caches the path.
    let original = create_flow(
        &dir,
        "v1".to_string(),
        trigger_graph(),
        true, // require_approval — the migrated column we'll drop below
        true,
    )
    .unwrap();
    assert!(original.require_approval);

    // Simulate a workspace restore of an OLDER database swapped in under the
    // same (already-cached) path: drop a migrated column and clear the version
    // stamp, exactly as a pre-migration database would look on disk.
    let db_path = dir.join("flows.db");
    {
        let raw = rusqlite::Connection::open(&db_path).unwrap();
        raw.execute_batch(
            "ALTER TABLE flow_definitions DROP COLUMN require_approval;
             PRAGMA user_version = 0;",
        )
        .unwrap();
    }

    // The path is still cached. With the old single-table `sqlite_master` probe
    // this database would be trusted and `list_flows` (which selects
    // `require_approval`) would fail with `no such column`. The version check
    // detects the drift and re-migrates via the idempotent `init_schema`.
    let (listed, skipped) = list_flows(&dir)
        .expect("an older on-disk schema under a cached path must be re-migrated, not trusted");
    assert_eq!(skipped, 0);
    assert_eq!(
        listed.len(),
        1,
        "the pre-existing row survives DROP COLUMN and the schema is repaired"
    );
    assert_eq!(listed[0].id, original.id);
    // The migrated column is back, reading its default for the pre-existing row.
    let reloaded = get_flow(&dir, &original.id).unwrap().unwrap();
    assert!(
        !reloaded.require_approval,
        "the re-added column defaults to 0 for the pre-existing row"
    );

    // And the store is fully usable again, not merely readable.
    let recreated = create_flow(&dir, "v2".to_string(), trigger_graph(), false, true)
        .expect("writes must work against the re-migrated schema");
    assert_ne!(recreated.id, original.id);
    let (flows_final, _) = list_flows(&dir).unwrap();
    assert_eq!(flows_final.len(), 2);
}
