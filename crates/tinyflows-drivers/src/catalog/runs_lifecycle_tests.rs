//! A run lifecycle driven through two catalog handles on one backend.

use super::*;
use crate::catalog::test_support::trigger_graph;
use tinyflows::nodes::control_flow::dedup::{committed_key, tentative_key};
use tinyflows::nodes::control_flow::dedup_settle;
use tinystoragedrivers_testkit::MemoryBackend as MemoryStorage;

fn step(node: &str) -> FlowRunStep {
    FlowRunStep {
        node_id: node.into(),
        ..Default::default()
    }
}

/// Two handles on one backend, as two processes sharing one database would
/// be: park through one, resume and settle through the other, expire through
/// the first, and read everything back through both.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_handles_on_one_backend_agree_on_a_run_lifecycle() {
    use crate::catalog::FlowStateDocuments;
    use tinyflows::caps::StateStore;

    let storage = MemoryStorage::new();
    let a = crate::catalog::test_support::catalog_in(&storage, "tenant");
    let b = crate::catalog::test_support::catalog_in(&storage, "tenant");
    let flow = a
        .create_flow("f".into(), trigger_graph(), true, true)
        .await
        .unwrap()
        .id;

    a.insert_flow_run("r1", &flow, "r1", "2026-01-01T00:00:00Z")
        .await
        .unwrap();
    a.upsert_flow_run_step("r1", &step("t")).await.unwrap();
    a.finish_flow_run(
        "r1",
        "pending_approval",
        "2026-01-01T00:00:01Z",
        &[step("t")],
        &["send".into()],
        None,
        Some("h"),
    )
    .await
    .unwrap();
    let state = FlowStateDocuments::new(a.clone(), flow.clone());
    // What the `dedup` node writes during the run, through the engine's
    // `StateStore`.
    state
        .store(&tentative_key("d", None), json!(["k1"]))
        .await
        .unwrap();
    state
        .store(&committed_key("d"), json!(["k0"]))
        .await
        .unwrap();

    assert!(b.mark_run_resuming("r1").await.unwrap());
    assert!(
        !a.mark_run_resuming("r1").await.unwrap(),
        "the other handle sees the resume"
    );
    b.upsert_flow_run_step("r1", &step("send")).await.unwrap();
    assert!(
        b.finish_flow_run(
            "r1",
            "completed",
            "2026-01-01T00:00:05Z",
            &[step("t"), step("send")],
            &[],
            None,
            None
        )
        .await
        .unwrap()
    );
    // The host's real settlement, through the other handle's `DedupKv`.
    let settle = FlowStateDocuments::new(b.clone(), flow.clone());
    let outcome =
        tokio::task::spawn_blocking(move || dedup_settle::settle(&settle, "d", None, true))
            .await
            .unwrap();
    assert!(
        matches!(
            outcome,
            dedup_settle::Settlement::Commit(dedup_settle::CommitOutcome::Committed {
                added: 1,
                committed_len: 2,
                clear_error: None,
            })
        ),
        "{outcome:?}"
    );

    a.insert_flow_run("r2", &flow, "r2", "2026-01-02T00:00:00Z")
        .await
        .unwrap();
    a.finish_flow_run(
        "r2",
        "pending_approval",
        "2026-01-02T00:00:01Z",
        &[],
        &["send".into()],
        None,
        None,
    )
    .await
    .unwrap();
    let swept = a
        .expire_parked_runs("2026-01-03T00:00:00Z", "2026-01-03T00:00:00Z", "ttl")
        .await
        .unwrap();
    assert_eq!(swept, [("r2".to_string(), flow.clone())]);

    for handle in [&a, &b] {
        let runs = handle.list_flow_runs(&flow, 10).await.unwrap();
        let summary: Vec<(&str, &str, usize)> = runs
            .iter()
            .map(|r| (r.id.as_str(), r.status.as_str(), r.steps.len()))
            .collect();
        assert_eq!(summary, [("r2", "cancelled", 0), ("r1", "completed", 2)]);
        let committed = handle
            .kv_get(&flow, &committed_key("d"))
            .await
            .unwrap()
            .unwrap();
        let mut committed: Vec<&str> = committed
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        committed.sort_unstable();
        assert_eq!(committed, ["k0", "k1"]);
        assert!(
            handle
                .kv_get(&flow, &tentative_key("d", None))
                .await
                .unwrap()
                .is_none()
        );
    }
}

/// A resume claim and a parked-run cancellation must race through one atomic
/// status check. A finish that was authorized while the row was parked must
/// not settle it after `mark_run_resuming` has changed it to `running`.
#[tokio::test]
async fn parked_finish_does_not_settle_a_run_claimed_for_resume() {
    let store = crate::catalog::test_support::catalog();
    let flow = store
        .create_flow("f".into(), trigger_graph(), true, true)
        .await
        .unwrap()
        .id;
    store
        .insert_flow_run("race", &flow, "race", "2026-01-01T00:00:00Z")
        .await
        .unwrap();
    assert!(
        store
            .finish_flow_run(
                "race",
                "pending_approval",
                "2026-01-01T00:00:01Z",
                &[],
                &["gate".into()],
                None,
                Some("graph-hash"),
            )
            .await
            .unwrap()
    );

    // Resume B wins after a cancellation or stale-graph check observed the
    // parked row but before its guarded settle reaches storage.
    assert!(store.mark_run_resuming("race").await.unwrap());
    assert!(
        !store
            .finish_parked_flow_run(
                "race",
                "cancelled",
                "2026-01-01T00:00:02Z",
                &[],
                &[],
                Some("stale observer"),
                None,
            )
            .await
            .unwrap()
    );
    assert_eq!(
        store.get_flow_run("race").await.unwrap().unwrap().status,
        "running"
    );
}
