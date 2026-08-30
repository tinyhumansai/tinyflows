//! Tests for concurrent incremental step persistence.

use super::*;
use crate::flows::definitions::create_flow;
use crate::flows::runs::{get_flow_run, insert_flow_run};
use crate::flows::test_support::*;
use tempfile::TempDir;

#[test]
fn concurrent_step_upserts_do_not_lose_a_step() {
    // Two observer callbacks for parallel branch nodes of the same run,
    // racing to persist their step. Before the `BEGIN IMMEDIATE` fix this was
    // a classic untransacted read-modify-write: both threads could read the
    // same pre-write `steps_json`, and whichever `UPDATE` landed last would
    // silently discard the other thread's step — permanently, since the
    // post-hoc `settle_steps` reconstruction only refills a missing node with
    // `status: None`, not its real outcome.
    let tmp = TempDir::new().unwrap();
    let dir = test_dir(&tmp);
    let flow = create_flow(&dir, "demo".to_string(), trigger_graph(), false, true).unwrap();
    let run_id = "run-concurrent";
    insert_flow_run(&dir, run_id, &flow.id, run_id, "2026-01-01T00:00:00Z").unwrap();

    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));

    let dir_a = dir.clone();
    let barrier_a = barrier.clone();
    let handle_a = std::thread::spawn(move || {
        barrier_a.wait();
        upsert_flow_run_step(
            &dir_a,
            run_id,
            &FlowRunStep {
                node_id: "branch-a".to_string(),
                output: serde_json::json!([{"json": {"a": 1}}]),
                status: Some("success".to_string()),
                ..Default::default()
            },
        )
    });

    let dir_b = dir.clone();
    let barrier_b = barrier.clone();
    let handle_b = std::thread::spawn(move || {
        barrier_b.wait();
        upsert_flow_run_step(
            &dir_b,
            run_id,
            &FlowRunStep {
                node_id: "branch-b".to_string(),
                output: serde_json::json!([{"json": {"b": 1}}]),
                status: Some("success".to_string()),
                ..Default::default()
            },
        )
    });

    handle_a.join().unwrap().unwrap();
    handle_b.join().unwrap().unwrap();

    let row = get_flow_run(&dir, run_id).unwrap().unwrap();
    let node_ids: std::collections::HashSet<&str> =
        row.steps.iter().map(|s| s.node_id.as_str()).collect();
    assert_eq!(
        row.steps.len(),
        2,
        "both concurrent steps must survive, none silently dropped: {:?}",
        row.steps
    );
    assert!(node_ids.contains("branch-a"));
    assert!(node_ids.contains("branch-b"));
}

#[test]
fn concurrent_upserts_to_the_same_node_id_do_not_corrupt_the_step_list() {
    // Same run, same node_id, racing "replace" writes — the transaction must
    // still leave exactly one entry for that node (whichever write wins the
    // serialization order), never a torn/duplicated list.
    let tmp = TempDir::new().unwrap();
    let dir = test_dir(&tmp);
    let flow = create_flow(&dir, "demo".to_string(), trigger_graph(), false, true).unwrap();
    let run_id = "run-same-node";
    insert_flow_run(&dir, run_id, &flow.id, run_id, "2026-01-01T00:00:00Z").unwrap();

    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let mut handles = Vec::new();
    for i in 0..2 {
        let dir = dir.clone();
        let barrier = barrier.clone();
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            upsert_flow_run_step(
                &dir,
                run_id,
                &FlowRunStep {
                    node_id: "same-node".to_string(),
                    output: serde_json::json!([{"json": {"attempt": i}}]),
                    status: Some("success".to_string()),
                    ..Default::default()
                },
            )
        }));
    }
    for h in handles {
        h.join().unwrap().unwrap();
    }

    let row = get_flow_run(&dir, run_id).unwrap().unwrap();
    assert_eq!(
        row.steps.len(),
        1,
        "a re-upsert of the same node_id must replace, not duplicate: {:?}",
        row.steps
    );
    assert_eq!(row.steps[0].node_id, "same-node");
}

// ── R-m8: schema init is gated to once per process per database path ───────
