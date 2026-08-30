//! Coverage for the host-owned SQLite checkpointer.
//!
//! The port's whole value is that it is *not* new code: the durable flow-run
//! database predates it, and a run interrupted before the upgrade has to
//! resume after it. So the tests here are about equivalence and durability
//! rather than about the SQL — `schema_is_identical_to_the_backend_it_replaced`
//! is the load-bearing one, and the rest exercise the trait surface
//! `flows_run` / `flows_resume` actually drive.

use serde_json::json;
use tinyflows::graph::ids::NodeId;
use tinyflows::graph::{Checkpoint, CheckpointConfig, Checkpointer, PendingWrite};

use crate::checkpoint::SqliteCheckpointer;

/// One checkpoint in `thread`, at `step`, chained to `parent`.
fn checkpoint(
    thread: &str,
    id: &str,
    parent: Option<&str>,
    step: u64,
    state: serde_json::Value,
) -> Checkpoint<serde_json::Value> {
    Checkpoint {
        thread_id: thread.to_string(),
        checkpoint_id: id.to_string(),
        run_id: Some("run-1".to_string()),
        parent_checkpoint_id: parent.map(str::to_string),
        namespace: Vec::new(),
        state,
        next_nodes: vec![NodeId::new("next")],
        completed_tasks: vec![NodeId::new("done")],
        pending_writes: Vec::new(),
        interrupts: Vec::new(),
        pending_activations: None,
        barrier_arrivals: Vec::new(),
        metadata: json!({ "source": "loop", "step": step }),
    }
}

/// `get(None)` is what resume calls, and "latest" has to mean latest by
/// insertion, not by id — the ids are opaque and nothing orders them.
#[tokio::test]
async fn get_without_an_id_returns_the_most_recently_written_checkpoint() {
    let store = SqliteCheckpointer::<serde_json::Value>::in_memory().unwrap();
    store
        .put(checkpoint("t1", "cp-1", None, 1, json!({ "n": 1 })))
        .await
        .unwrap();
    store
        .put(checkpoint("t1", "cp-2", Some("cp-1"), 2, json!({ "n": 2 })))
        .await
        .unwrap();

    let latest = store.get("t1", None).await.unwrap().expect("latest");
    assert_eq!(latest.checkpoint_id, "cp-2");
    assert_eq!(latest.state, json!({ "n": 2 }));

    let addressed = store.get("t1", Some("cp-1")).await.unwrap().expect("cp-1");
    assert_eq!(addressed.state, json!({ "n": 1 }));

    assert!(store.get("t1", Some("nope")).await.unwrap().is_none());
    assert!(store.get("other-thread", None).await.unwrap().is_none());
}

/// Threads must not see each other: a flow run addresses its own thread id,
/// and one flow's checkpoints leaking into another's resume would replay the
/// wrong graph.
#[tokio::test]
async fn threads_are_isolated_and_listed_in_insertion_order() {
    let store = SqliteCheckpointer::<serde_json::Value>::in_memory().unwrap();
    store
        .put(checkpoint("t1", "a", None, 1, json!(1)))
        .await
        .unwrap();
    store
        .put(checkpoint("t2", "b", None, 1, json!(2)))
        .await
        .unwrap();
    store
        .put(checkpoint("t1", "c", Some("a"), 2, json!(3)))
        .await
        .unwrap();

    let listed: Vec<String> = store
        .list("t1")
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.checkpoint_id)
        .collect();
    assert_eq!(listed, vec!["a".to_string(), "c".to_string()]);

    let mut threads = store.list_threads().await.unwrap();
    threads.sort();
    assert_eq!(threads, vec!["t1".to_string(), "t2".to_string()]);
}

/// A namespaced read is how a parent run and the sub-workflows it embeds stay
/// out of each other's checkpoints — they share a thread id and differ only
/// here.
#[tokio::test]
async fn scoped_reads_see_only_their_own_namespace() {
    let store = SqliteCheckpointer::<serde_json::Value>::in_memory().unwrap();
    let mut root = checkpoint("t1", "root-1", None, 1, json!("root"));
    root.namespace = Vec::new();
    let mut child = checkpoint("t1", "child-1", None, 1, json!("child"));
    child.namespace = vec!["sub".to_string()];
    store.put(root).await.unwrap();
    store.put(child).await.unwrap();

    let scoped = store
        .get_scoped("t1", None, &["sub".to_string()])
        .await
        .unwrap()
        .expect("child checkpoint");
    assert_eq!(scoped.state, json!("child"));

    let at_root = store
        .get_scoped("t1", None, &[])
        .await
        .unwrap()
        .expect("root checkpoint");
    assert_eq!(at_root.state, json!("root"));
}

/// The partial-failure ledger, and its replace-vs-ignore rule — the thing in
/// this file most likely to be got subtly wrong, because both halves look like
/// "write it again".
///
/// A **data** write (`idx >= 0`) is append-once: a superstep that is retried
/// after a partial failure re-emits what it already emitted, and taking the
/// second copy would fold the same emission twice on resume. A
/// **control-plane** write (`idx < 0`, e.g. a resume value) legitimately
/// changes on a retry and must upsert. Both are pushed into SQL as two
/// conflict clauses rather than a read-then-write, so they stay correct under
/// concurrent writers.
#[tokio::test]
async fn data_writes_are_append_once_and_control_plane_writes_upsert() {
    let store = SqliteCheckpointer::<serde_json::Value>::in_memory().unwrap();
    store
        .put(checkpoint("t1", "cp-1", None, 1, json!({})))
        .await
        .unwrap();
    let config = CheckpointConfig {
        thread_id: "t1".to_string(),
        checkpoint_id: Some("cp-1".to_string()),
        namespace: Vec::new(),
    };

    // Data write, then the same (task_id, idx) again: the first value stands.
    store
        .put_writes(
            &config,
            &[PendingWrite::data("n1", "task-a", 0, "out", json!("first"))],
        )
        .await
        .unwrap();
    store
        .put_writes(
            &config,
            &[PendingWrite::data(
                "n1",
                "task-a",
                0,
                "out",
                json!("second"),
            )],
        )
        .await
        .unwrap();

    let writes = store.get_writes(&config).await.unwrap();
    assert_eq!(
        writes.len(),
        1,
        "a re-run task must not duplicate its write"
    );
    assert_eq!(
        writes[0].payload,
        json!("first"),
        "a data write is append-once — a retry must not overwrite what already landed"
    );

    // Control-plane write at the reserved resume index: the newest value wins.
    let resume = |payload| {
        PendingWrite::data(
            "n1",
            "task-a",
            tinyflows::graph::checkpoint::WRITES_IDX_RESUME,
            "__resume__",
            payload,
        )
    };
    store
        .put_writes(&config, &[resume(json!("old"))])
        .await
        .unwrap();
    store
        .put_writes(&config, &[resume(json!("new"))])
        .await
        .unwrap();

    let writes = store.get_writes(&config).await.unwrap();
    let control: Vec<_> = writes.iter().filter(|w| w.is_control_plane()).collect();
    assert_eq!(
        control.len(),
        1,
        "control-plane writes are keyed, not appended"
    );
    assert_eq!(
        control[0].payload,
        json!("new"),
        "a control-plane write must upsert — a resume value changes on a retry"
    );
}

/// Deleting a thread is how a flow's history is dropped when the flow is
/// deleted; it must take that thread's writes with it and leave every other
/// thread alone.
#[tokio::test]
async fn deleting_a_thread_removes_its_checkpoints_and_leaves_others() {
    let store = SqliteCheckpointer::<serde_json::Value>::in_memory().unwrap();
    store
        .put(checkpoint("doomed", "a", None, 1, json!(1)))
        .await
        .unwrap();
    store
        .put(checkpoint("kept", "b", None, 1, json!(2)))
        .await
        .unwrap();

    store.delete_thread("doomed").await.unwrap();

    assert!(store.get("doomed", None).await.unwrap().is_none());
    assert!(store.list("doomed").await.unwrap().is_empty());
    assert!(store.get("kept", None).await.unwrap().is_some());
}

/// Pruning bounds a long-running flow's history. It keeps the newest N, which
/// is the end resume reads from.
#[tokio::test]
async fn prune_keeps_the_newest_checkpoints() {
    let store = SqliteCheckpointer::<serde_json::Value>::in_memory().unwrap();
    for i in 1..=5 {
        store
            .put(checkpoint("t1", &format!("cp-{i}"), None, i, json!(i)))
            .await
            .unwrap();
    }

    let removed = store.prune("t1", 2).await.unwrap();
    assert_eq!(removed, 3);

    let remaining: Vec<String> = store
        .list("t1")
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.checkpoint_id)
        .collect();
    assert_eq!(remaining, vec!["cp-4".to_string(), "cp-5".to_string()]);
}

/// An in-memory database lives on its connection, so clones have to share one
/// — a clone that silently got its own empty database would be a resume that
/// finds nothing.
#[tokio::test]
async fn clones_share_one_in_memory_database() {
    let store = SqliteCheckpointer::<serde_json::Value>::in_memory().unwrap();
    let clone = store.clone();
    store
        .put(checkpoint(
            "t1",
            "cp-1",
            None,
            1,
            json!("written via original"),
        ))
        .await
        .unwrap();

    let seen = clone.get("t1", None).await.unwrap().expect("shared data");
    assert_eq!(seen.state, json!("written via original"));
}
