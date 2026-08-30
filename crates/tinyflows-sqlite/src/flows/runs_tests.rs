//! Tests for `flow_runs` CRUD, pruning, and the parked-run expiry /
//! resume-tracking helpers.

use super::*;
use crate::flows::definitions::create_flow;
use crate::flows::test_support::*;
use tempfile::TempDir;

#[test]
fn flow_run_insert_finish_get_round_trip() {
    let tmp = TempDir::new().unwrap();
    let dir = test_dir(&tmp);
    let flow = create_flow(&dir, "demo".to_string(), trigger_graph(), false, true).unwrap();

    let thread_id = format!("flow:{}:run-1", flow.id);
    insert_flow_run(
        &dir,
        &thread_id,
        &flow.id,
        &thread_id,
        "2026-01-01T00:00:00Z",
    )
    .unwrap();

    let running = get_flow_run(&dir, &thread_id)
        .unwrap()
        .expect("row present");
    assert_eq!(running.status, "running");
    assert!(running.finished_at.is_none());
    assert!(running.steps.is_empty());

    let steps = vec![FlowRunStep {
        node_id: "t".to_string(),
        output: serde_json::json!([{"json": {"x": 1}}]),
        port: None,
        ..Default::default()
    }];
    finish_flow_run(
        &dir,
        &thread_id,
        "completed",
        "2026-01-01T00:00:01Z",
        &steps,
        &[],
        None,
        None,
    )
    .unwrap();

    let finished = get_flow_run(&dir, &thread_id)
        .unwrap()
        .expect("row present");
    assert_eq!(finished.status, "completed");
    assert_eq!(
        finished.finished_at.as_deref(),
        Some("2026-01-01T00:00:01Z")
    );
    assert_eq!(finished.steps.len(), 1);
    assert_eq!(finished.steps[0].node_id, "t");
    assert!(finished.pending_approvals.is_empty());
    assert!(finished.error.is_none());
}

#[test]
fn finish_flow_run_records_error_on_failure() {
    let tmp = TempDir::new().unwrap();
    let dir = test_dir(&tmp);
    let flow = create_flow(&dir, "demo".to_string(), trigger_graph(), false, true).unwrap();
    let thread_id = format!("flow:{}:run-2", flow.id);
    insert_flow_run(
        &dir,
        &thread_id,
        &flow.id,
        &thread_id,
        "2026-01-01T00:00:00Z",
    )
    .unwrap();

    finish_flow_run(
        &dir,
        &thread_id,
        "failed",
        "2026-01-01T00:00:01Z",
        &[],
        &[],
        Some("boom"),
        None,
    )
    .unwrap();

    let finished = get_flow_run(&dir, &thread_id).unwrap().unwrap();
    assert_eq!(finished.status, "failed");
    assert_eq!(finished.error.as_deref(), Some("boom"));
}

#[test]
fn get_flow_run_returns_none_for_unknown_id() {
    let tmp = TempDir::new().unwrap();
    let dir = test_dir(&tmp);
    assert!(get_flow_run(&dir, "missing").unwrap().is_none());
}

#[test]
fn list_flow_runs_orders_newest_first_and_is_scoped_to_flow() {
    let tmp = TempDir::new().unwrap();
    let dir = test_dir(&tmp);
    let flow_a = create_flow(&dir, "a".to_string(), trigger_graph(), false, true).unwrap();
    let flow_b = create_flow(&dir, "b".to_string(), trigger_graph(), false, true).unwrap();

    insert_flow_run(&dir, "run-a1", &flow_a.id, "run-a1", "2026-01-01T00:00:00Z").unwrap();
    insert_flow_run(&dir, "run-a2", &flow_a.id, "run-a2", "2026-01-02T00:00:00Z").unwrap();
    insert_flow_run(&dir, "run-b1", &flow_b.id, "run-b1", "2026-01-01T00:00:00Z").unwrap();

    let runs_a = list_flow_runs(&dir, &flow_a.id, 10).unwrap();
    assert_eq!(runs_a.len(), 2);
    assert_eq!(runs_a[0].id, "run-a2", "newest run must come first");
    assert_eq!(runs_a[1].id, "run-a1");

    let runs_b = list_flow_runs(&dir, &flow_b.id, 10).unwrap();
    assert_eq!(runs_b.len(), 1);
    assert_eq!(runs_b[0].id, "run-b1");
}

// ── insert_duplicate_flow ─────────────────────────────────────────────────

fn seed_run(dir: &Path, flow_id: &str, id: &str, day: u32, status: &str) {
    let started = format!("2026-01-{day:02}T00:00:00Z");
    insert_flow_run(dir, id, flow_id, id, &started).unwrap();
    if status != "running" {
        finish_flow_run(
            dir,
            id,
            status,
            &format!("2026-01-{day:02}T00:00:05Z"),
            &[],
            &[],
            None,
            None,
        )
        .unwrap();
    }
}

#[test]
fn prune_flow_runs_keeps_newest_n_terminal_runs() {
    let tmp = TempDir::new().unwrap();
    let dir = test_dir(&tmp);
    let flow = create_flow(&dir, "demo".to_string(), trigger_graph(), false, true).unwrap();

    // 5 completed runs on ascending days.
    for i in 1..=5 {
        seed_run(&dir, &flow.id, &format!("run-{i}"), i, "completed");
    }

    let deleted = prune_flow_runs(&dir, &flow.id, 2).unwrap();
    assert_eq!(deleted, 3, "5 terminal runs, keep 2 => 3 pruned");

    let remaining = list_flow_runs(&dir, &flow.id, 100).unwrap();
    let ids: Vec<_> = remaining.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, vec!["run-5", "run-4"], "newest two survive");
}

#[test]
fn prune_flow_runs_never_removes_pending_approval_run() {
    let tmp = TempDir::new().unwrap();
    let dir = test_dir(&tmp);
    let flow = create_flow(&dir, "demo".to_string(), trigger_graph(), false, true).unwrap();

    // An OLD parked pending_approval run (day 1) plus newer completed runs.
    seed_run(&dir, &flow.id, "parked", 1, "pending_approval");
    for i in 2..=5 {
        seed_run(&dir, &flow.id, &format!("run-{i}"), i, "completed");
    }

    // keep=1 would normally leave only the newest run; the parked one must
    // still survive despite being the oldest and outside the newest-1 window.
    let deleted = prune_flow_runs(&dir, &flow.id, 1).unwrap();
    let remaining = list_flow_runs(&dir, &flow.id, 100).unwrap();
    let ids: std::collections::HashSet<_> = remaining.iter().map(|r| r.id.as_str()).collect();
    assert!(
        ids.contains("parked"),
        "a pending_approval run must never be pruned out from under a resume"
    );
    assert!(ids.contains("run-5"), "newest terminal run kept");
    // Only terminal runs 2..4 were eligible; 5 kept by window => 3 deleted.
    assert_eq!(deleted, 3);
}

#[test]
fn prune_flow_runs_leaves_running_rows_alone() {
    let tmp = TempDir::new().unwrap();
    let dir = test_dir(&tmp);
    let flow = create_flow(&dir, "demo".to_string(), trigger_graph(), false, true).unwrap();

    seed_run(&dir, &flow.id, "live", 1, "running");
    for i in 2..=4 {
        seed_run(&dir, &flow.id, &format!("run-{i}"), i, "completed");
    }

    prune_flow_runs(&dir, &flow.id, 1).unwrap();
    let remaining = list_flow_runs(&dir, &flow.id, 100).unwrap();
    let ids: std::collections::HashSet<_> = remaining.iter().map(|r| r.id.as_str()).collect();
    assert!(ids.contains("live"), "a running run is never pruned");
}

#[test]
fn insert_flow_run_auto_prunes_beyond_retention_cap() {
    let tmp = TempDir::new().unwrap();
    let dir = test_dir(&tmp);
    let flow = create_flow(&dir, "demo".to_string(), trigger_graph(), false, true).unwrap();

    // Seed exactly MAX_FLOW_RUNS_PER_FLOW completed runs.
    let cap = MAX_FLOW_RUNS_PER_FLOW;
    for i in 0..cap {
        let id = format!("run-{i:04}");
        insert_flow_run(
            &dir,
            &id,
            &flow.id,
            &id,
            &format!("2026-01-01T00:00:{i:02}Z"),
        )
        .unwrap();
        finish_flow_run(
            &dir,
            &id,
            "completed",
            "2026-01-01T00:01:00Z",
            &[],
            &[],
            None,
            None,
        )
        .unwrap();
    }
    assert_eq!(list_flow_runs(&dir, &flow.id, cap * 2).unwrap().len(), cap);

    // One more insert should trigger the retention prune, keeping <= cap.
    let extra = "run-extra";
    insert_flow_run(&dir, extra, &flow.id, extra, "2026-01-02T00:00:00Z").unwrap();
    let count = list_flow_runs(&dir, &flow.id, cap * 2).unwrap().len();
    assert!(
        count <= cap,
        "auto-prune should keep run count within cap ({count} > {cap})"
    );
}

#[test]
fn list_flow_runs_respects_limit() {
    let tmp = TempDir::new().unwrap();
    let dir = test_dir(&tmp);
    let flow = create_flow(&dir, "demo".to_string(), trigger_graph(), false, true).unwrap();

    for i in 0..3 {
        let id = format!("run-{i}");
        insert_flow_run(
            &dir,
            &id,
            &flow.id,
            &id,
            &format!("2026-01-0{}T00:00:00Z", i + 1),
        )
        .unwrap();
    }

    let limited = list_flow_runs(&dir, &flow.id, 2).unwrap();
    assert_eq!(limited.len(), 2);
}

// ── flow_suggestions ─────────────────────────────────────────────────────────

#[test]
fn list_running_run_ids_returns_only_running_rows() {
    let tmp = TempDir::new().unwrap();
    let dir = test_dir(&tmp);
    let flow = create_flow(&dir, "demo".to_string(), trigger_graph(), false, true).unwrap();

    insert_flow_run(
        &dir,
        "run-live-1",
        &flow.id,
        "run-live-1",
        "2026-01-01T00:00:00Z",
    )
    .unwrap();
    insert_flow_run(
        &dir,
        "run-live-2",
        &flow.id,
        "run-live-2",
        "2026-01-01T00:00:01Z",
    )
    .unwrap();
    insert_flow_run(
        &dir,
        "run-done",
        &flow.id,
        "run-done",
        "2026-01-01T00:00:02Z",
    )
    .unwrap();
    finish_flow_run(
        &dir,
        "run-done",
        "completed",
        "2026-01-01T00:00:03Z",
        &[],
        &[],
        None,
        None,
    )
    .unwrap();

    let mut running = list_running_run_ids(&dir, "2099-01-01T00:00:00Z").unwrap();
    running.sort();
    assert_eq!(
        running,
        vec![
            ("run-live-1".to_string(), flow.id.clone()),
            ("run-live-2".to_string(), flow.id.clone()),
        ],
        "only the two still-running rows must be listed, not the completed one"
    );
}

#[test]
fn list_running_run_ids_excludes_rows_started_at_or_after_the_floor() {
    let tmp = TempDir::new().unwrap();
    let dir = test_dir(&tmp);
    let flow = create_flow(&dir, "demo".to_string(), trigger_graph(), false, true).unwrap();

    insert_flow_run(&dir, "run-old", &flow.id, "run-old", "2026-01-01T00:00:00Z").unwrap();
    insert_flow_run(&dir, "run-at", &flow.id, "run-at", "2026-01-01T00:00:05Z").unwrap();
    insert_flow_run(&dir, "run-new", &flow.id, "run-new", "2026-01-01T00:00:09Z").unwrap();

    // The floor is exclusive: a row stamped exactly at the boot floor was
    // inserted by THIS process (`start_flow_run_row` anchors the floor before
    // stamping), so it must fall outside the candidate set along with newer
    // rows — otherwise the sweep could interrupt a live run and drop its
    // checkpoint mid-flight.
    let running = list_running_run_ids(&dir, "2026-01-01T00:00:05Z").unwrap();
    assert_eq!(
        running,
        vec![("run-old".to_string(), flow.id.clone())],
        "only rows strictly older than the floor are sweep candidates"
    );
}

#[test]
fn mark_run_interrupted_reconciles_a_running_row_with_reason() {
    let tmp = TempDir::new().unwrap();
    let dir = test_dir(&tmp);
    let flow = create_flow(&dir, "demo".to_string(), trigger_graph(), false, true).unwrap();
    insert_flow_run(&dir, "run-x", &flow.id, "run-x", "2026-01-01T00:00:00Z").unwrap();

    let flipped =
        mark_run_interrupted(&dir, "run-x", "2026-01-01T00:05:00Z", "boom reason").unwrap();
    assert!(flipped, "a running row must be reconciled");

    let row = get_flow_run(&dir, "run-x").unwrap().unwrap();
    assert_eq!(row.status, "interrupted");
    assert_eq!(row.finished_at.as_deref(), Some("2026-01-01T00:05:00Z"));
    assert_eq!(row.error.as_deref(), Some("boom reason"));
}

#[test]
fn mark_run_interrupted_is_a_noop_for_a_terminal_row() {
    let tmp = TempDir::new().unwrap();
    let dir = test_dir(&tmp);
    let flow = create_flow(&dir, "demo".to_string(), trigger_graph(), false, true).unwrap();
    insert_flow_run(&dir, "run-y", &flow.id, "run-y", "2026-01-01T00:00:00Z").unwrap();
    finish_flow_run(
        &dir,
        "run-y",
        "completed",
        "2026-01-01T00:00:01Z",
        &[],
        &[],
        None,
        None,
    )
    .unwrap();

    // The `status = 'running'` guard must protect an already-settled run.
    let flipped =
        mark_run_interrupted(&dir, "run-y", "2026-01-01T00:05:00Z", "should not apply").unwrap();
    assert!(
        !flipped,
        "a completed run must never be clobbered to interrupted"
    );

    let row = get_flow_run(&dir, "run-y").unwrap().unwrap();
    assert_eq!(row.status, "completed");
    assert!(row.error.is_none());
}

/// `expire_parked_runs` must return only the runs it ACTUALLY flipped, not the
/// candidates its `SELECT` saw.
///
/// The `SELECT` and each row's guarded `UPDATE` are separate statements on an
/// autocommit connection, so a concurrent `mark_run_resuming` can claim a row in
/// between. The per-row `WHERE status = 'pending_approval'` keeps that row safe,
/// but returning the unfiltered candidate list would let the caller act on a run
/// it never expired — dropping the checkpoint out from under a live resume and
/// publishing a terminal `FlowRunFinished` for a run still executing. That false
/// event is the worse half: the frontend de-dupes terminal events per
/// `flow_id:run_id`, so the run's real completion would later be discarded.
#[test]
fn expire_parked_runs_returns_only_rows_it_actually_flipped() {
    let tmp = TempDir::new().unwrap();
    let dir = test_dir(&tmp);
    let flow = create_flow(&dir, "ttl".to_string(), trigger_graph(), false, true).unwrap();

    let stale_at = "2000-01-01T00:00:00+00:00";
    for id in ["claimed-run", "genuinely-stale-run"] {
        insert_flow_run(&dir, id, &flow.id, id, stale_at).unwrap();
        finish_flow_run(
            &dir,
            id,
            "pending_approval",
            stale_at,
            &[],
            &["gate".to_string()],
            None,
            // No graph pin (T-M1): this fixture is about the TTL sweep's
            // candidates-vs-sweeps behaviour, not stale-approval detection, so
            // these rows stand in for pre-pin legacy parks.
            None,
        )
        .unwrap();
    }

    // Simulate the race: one candidate is claimed by a resume after the sweep's
    // SELECT would have seen it, but before its UPDATE lands.
    assert!(mark_run_resuming(&dir, "claimed-run").unwrap());

    let swept = expire_parked_runs(
        &dir,
        "2099-01-01T00:00:00+00:00",
        "2026-01-01T00:00:00+00:00",
        "expired",
    )
    .unwrap();

    let swept_ids: Vec<&str> = swept.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(
        swept_ids,
        vec!["genuinely-stale-run"],
        "only the row whose guarded UPDATE matched may be reported as swept"
    );
    assert_eq!(
        get_flow_run(&dir, "claimed-run").unwrap().unwrap().status,
        "running",
        "the claimed run must keep executing, untouched by the sweep"
    );
    assert_eq!(
        get_flow_run(&dir, "genuinely-stale-run")
            .unwrap()
            .unwrap()
            .status,
        "cancelled"
    );
}

// ── R-M4: corrupt/unmigratable graph_json rows must not brick a list ────────
