//! Tests for `flow_definitions` CRUD, listing, and last-run bookkeeping.

use super::*;
use crate::flows::runs::force_corrupt_graph_json_for_test;
use crate::flows::test_support::*;
use tempfile::TempDir;

#[test]
fn create_get_list_delete_roundtrip() {
    let tmp = TempDir::new().unwrap();
    let dir = test_dir(&tmp);

    let flow = create_flow(&dir, "demo".to_string(), trigger_graph(), false, true).unwrap();
    assert_eq!(flow.name, "demo");
    assert!(flow.enabled);

    let fetched = get_flow(&dir, &flow.id).unwrap().expect("flow present");
    assert_eq!(fetched.id, flow.id);
    assert_eq!(fetched.graph, flow.graph);

    let (listed, skipped) = list_flows(&dir).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, flow.id);
    assert_eq!(skipped, 0);

    remove_flow(&dir, &flow.id).unwrap();
    assert!(get_flow(&dir, &flow.id).unwrap().is_none());
    assert!(list_flows(&dir).unwrap().0.is_empty());
}

#[test]
fn get_flow_returns_none_for_unknown_id() {
    let tmp = TempDir::new().unwrap();
    let dir = test_dir(&tmp);
    assert!(get_flow(&dir, "missing").unwrap().is_none());
}

#[test]
fn remove_flow_errors_when_not_found() {
    let tmp = TempDir::new().unwrap();
    let dir = test_dir(&tmp);
    let err = remove_flow(&dir, "missing").unwrap_err();
    assert!(err.to_string().contains("not found"));
}

#[test]
fn set_enabled_toggles_and_persists() {
    let tmp = TempDir::new().unwrap();
    let dir = test_dir(&tmp);
    let flow = create_flow(&dir, "demo".to_string(), trigger_graph(), false, true).unwrap();
    assert!(flow.enabled);

    let disabled = set_enabled(&dir, &flow.id, false).unwrap();
    assert!(!disabled.enabled);

    let reloaded = get_flow(&dir, &flow.id).unwrap().unwrap();
    assert!(!reloaded.enabled);

    let enabled = set_enabled(&dir, &flow.id, true).unwrap();
    assert!(enabled.enabled);
}

#[test]
fn record_run_sets_last_run_fields() {
    let tmp = TempDir::new().unwrap();
    let dir = test_dir(&tmp);
    let flow = create_flow(&dir, "demo".to_string(), trigger_graph(), false, true).unwrap();
    assert!(flow.last_run_at.is_none());

    record_run(&dir, &flow.id, "completed").unwrap();
    let reloaded = get_flow(&dir, &flow.id).unwrap().unwrap();
    assert!(reloaded.last_run_at.is_some());
    assert_eq!(reloaded.last_status.as_deref(), Some("completed"));
}

#[test]
fn stored_graph_older_than_current_schema_is_migrated_on_read() {
    let tmp = TempDir::new().unwrap();
    let dir = test_dir(&tmp);

    // Insert a raw, versionless graph row directly (bypassing create_flow's
    // typed path) to simulate a definition persisted by an older crate build.
    let legacy_graph_json = serde_json::json!({
        "name": "legacy",
        "nodes": [{ "id": "t", "kind": "trigger", "name": "Trigger" }],
        "edges": []
    })
    .to_string();

    with_connection(&dir, |conn| {
        conn.execute(
            "INSERT INTO flow_definitions
                (id, name, graph_json, enabled, created_at, updated_at, last_run_at, last_status)
             VALUES ('legacy-1', 'legacy', ?1, 1, '2020-01-01T00:00:00Z', '2020-01-01T00:00:00Z', NULL, NULL)",
            rusqlite::params![legacy_graph_json],
        )?;
        Ok(())
    })
    .unwrap();

    let loaded = get_flow(&dir, "legacy-1").unwrap().expect("row present");
    assert_eq!(
        loaded.graph.schema_version,
        tinyflows::model::CURRENT_SCHEMA_VERSION
    );
    assert_eq!(loaded.graph.nodes.len(), 1);
}

#[test]
fn create_flow_persists_require_approval() {
    let tmp = TempDir::new().unwrap();
    let dir = test_dir(&tmp);

    let flow = create_flow(&dir, "demo".to_string(), trigger_graph(), true, true).unwrap();
    assert!(flow.require_approval);

    let reloaded = get_flow(&dir, &flow.id).unwrap().unwrap();
    assert!(reloaded.require_approval);
}

#[test]
fn legacy_flow_definitions_row_without_require_approval_column_defaults_false() {
    // A row inserted before the `require_approval` column existed. Schema
    // init (including the `add_column_if_missing` ALTER) runs once per
    // process per database file (R-m8) — since this test opens a fresh
    // per-`TempDir` database, that one-time init still runs here, simulating
    // a workspace opened once on an older build.
    let tmp = TempDir::new().unwrap();
    let dir = test_dir(&tmp);

    let legacy_graph_json = serde_json::to_string(&trigger_graph()).unwrap();
    with_connection(&dir, |conn| {
        conn.execute(
            "INSERT INTO flow_definitions
                (id, name, graph_json, enabled, created_at, updated_at, last_run_at, last_status)
             VALUES ('legacy-2', 'legacy', ?1, 1, '2020-01-01T00:00:00Z', '2020-01-01T00:00:00Z', NULL, NULL)",
            rusqlite::params![legacy_graph_json],
        )?;
        Ok(())
    })
    .unwrap();

    let loaded = get_flow(&dir, "legacy-2").unwrap().expect("row present");
    assert!(!loaded.require_approval);
}

// ── list_enabled_flows ────────────────────────────────────────────────────

#[test]
fn list_enabled_flows_excludes_disabled() {
    let tmp = TempDir::new().unwrap();
    let dir = test_dir(&tmp);

    let enabled_flow =
        create_flow(&dir, "enabled".to_string(), trigger_graph(), false, true).unwrap();
    let disabled_flow =
        create_flow(&dir, "disabled".to_string(), trigger_graph(), false, true).unwrap();
    set_enabled(&dir, &disabled_flow.id, false).unwrap();

    let (enabled, skipped) = list_enabled_flows(&dir).unwrap();
    assert_eq!(enabled.len(), 1);
    assert_eq!(enabled[0].id, enabled_flow.id);
    assert_eq!(skipped, 0);
}

// ── flow_runs CRUD ────────────────────────────────────────────────────────

#[test]
fn insert_duplicate_flow_makes_a_disabled_copy_with_new_id_and_same_graph() {
    let tmp = TempDir::new().unwrap();
    let dir = test_dir(&tmp);

    // Enabled source with require_approval + a distinctive graph name.
    let mut graph = trigger_graph();
    graph.name = "original-graph".to_string();
    let source = create_flow(&dir, "My Flow".to_string(), graph, true, true).unwrap();
    assert!(source.enabled);
    record_run(&dir, &source.id, "completed").unwrap();
    let source = get_flow(&dir, &source.id).unwrap().unwrap();
    assert!(source.last_status.is_some());

    let copy = insert_duplicate_flow(&dir, &source, "My Flow (copy)".to_string()).unwrap();

    // New id, suffixed name, DISABLED, run history reset.
    assert_ne!(copy.id, source.id);
    assert_eq!(copy.name, "My Flow (copy)");
    assert!(
        !copy.enabled,
        "duplicate must be disabled so it never fires"
    );
    assert!(copy.last_run_at.is_none());
    assert!(copy.last_status.is_none());
    // Same graph + require_approval carried over.
    assert_eq!(copy.graph, source.graph);
    assert_eq!(copy.graph.name, "original-graph");
    assert!(copy.require_approval);

    // Persisted and independent — both rows exist.
    let reloaded = get_flow(&dir, &copy.id).unwrap().unwrap();
    assert!(!reloaded.enabled);
    assert_eq!(reloaded.graph, source.graph);
    assert_eq!(list_flows(&dir).unwrap().0.len(), 2);
}

// ── prune_flow_runs ───────────────────────────────────────────────────────

#[test]
fn list_flows_skips_a_corrupt_row_and_reports_the_count() {
    let tmp = TempDir::new().unwrap();
    let dir = test_dir(&tmp);

    let good_a = create_flow(&dir, "good-a".to_string(), trigger_graph(), false, true).unwrap();
    let bad = create_flow(&dir, "bad".to_string(), trigger_graph(), false, true).unwrap();
    let good_b = create_flow(&dir, "good-b".to_string(), trigger_graph(), false, true).unwrap();
    force_corrupt_graph_json_for_test(&dir, &bad.id, "{ not even valid json").unwrap();

    let (flows, skipped) = list_flows(&dir).unwrap();
    assert_eq!(
        skipped, 1,
        "exactly the one corrupt row must be counted as skipped"
    );
    let ids: Vec<&str> = flows.iter().map(|f| f.id.as_str()).collect();
    assert_eq!(
        flows.len(),
        2,
        "the two good rows must still be returned: {ids:?}"
    );
    assert!(ids.contains(&good_a.id.as_str()));
    assert!(ids.contains(&good_b.id.as_str()));
    assert!(!ids.contains(&bad.id.as_str()));
}

#[test]
fn list_flows_skips_a_row_whose_schema_version_is_newer_than_this_build_supports() {
    // The real-world R-M4 scenario: a user ran a newer build that persisted a
    // graph at a `schema_version` this build's `tinyflows::migrate::migrate`
    // cannot step backward from, then downgraded.
    let tmp = TempDir::new().unwrap();
    let dir = test_dir(&tmp);

    let good = create_flow(&dir, "good".to_string(), trigger_graph(), false, true).unwrap();
    let too_new = create_flow(&dir, "too-new".to_string(), trigger_graph(), false, true).unwrap();
    let newer_schema_json = serde_json::json!({
        "schema_version": 999,
        "name": "from-the-future",
        "nodes": [],
        "edges": []
    })
    .to_string();
    force_corrupt_graph_json_for_test(&dir, &too_new.id, &newer_schema_json).unwrap();

    let (flows, skipped) = list_flows(&dir).unwrap();
    assert_eq!(skipped, 1);
    assert_eq!(flows.len(), 1);
    assert_eq!(flows[0].id, good.id);
}

#[test]
fn list_enabled_flows_still_returns_the_good_rows_when_one_is_corrupt() {
    // This is the blast-radius scenario R-M4 flags for `bus.rs::handle_app_event`:
    // `list_enabled_flows` backs ALL `app_event` trigger dispatch, so one
    // corrupt enabled flow must not blackhole matching for every other one.
    let tmp = TempDir::new().unwrap();
    let dir = test_dir(&tmp);

    let good = create_flow(&dir, "good".to_string(), trigger_graph(), false, true).unwrap();
    let bad = create_flow(&dir, "bad".to_string(), trigger_graph(), false, true).unwrap();
    force_corrupt_graph_json_for_test(&dir, &bad.id, "not json at all").unwrap();

    let (enabled, skipped) = list_enabled_flows(&dir).unwrap();
    assert_eq!(skipped, 1);
    assert_eq!(enabled.len(), 1);
    assert_eq!(enabled[0].id, good.id);
}

#[test]
fn list_enabled_flows_excludes_a_corrupt_disabled_row_without_counting_it_as_skipped() {
    // A corrupt row that was never enabled must not even be attempted for
    // decode by `list_enabled_flows` (the WHERE clause filters it out at the
    // SQL layer before `map_flow_row` ever runs) — it is neither returned nor
    // counted as skipped by this particular listing.
    let tmp = TempDir::new().unwrap();
    let dir = test_dir(&tmp);

    let good = create_flow(&dir, "good".to_string(), trigger_graph(), false, true).unwrap();
    let disabled_and_corrupt = create_flow(
        &dir,
        "disabled-bad".to_string(),
        trigger_graph(),
        false,
        true,
    )
    .unwrap();
    set_enabled(&dir, &disabled_and_corrupt.id, false).unwrap();
    force_corrupt_graph_json_for_test(&dir, &disabled_and_corrupt.id, "{{{").unwrap();

    let (enabled, skipped) = list_enabled_flows(&dir).unwrap();
    assert_eq!(skipped, 0);
    assert_eq!(enabled.len(), 1);
    assert_eq!(enabled[0].id, good.id);
}

// ── R-m1: concurrent step upserts must not lose a step ──────────────────────
