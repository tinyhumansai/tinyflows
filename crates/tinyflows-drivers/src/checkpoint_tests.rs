//! Behaviour of the storage-driver checkpointer, run against a driver document
//! store: the checkpointer contract every durable backend is held to, plus the
//! isolation and key-encoding guarantees the driver layout adds.

use super::*;
use crate::checkpoint_layout::{checkpoint_id_key, subgraph_key, thread_id_key};
use serde_json::json;
use tinyflows::graph::ids::NodeId;
use tinyflows_storage::{Precondition, Scope, StorageBackend, StorageError};
use tinystoragedrivers_testkit::MemoryBackend as MemoryStorage;

fn docs(storage: &MemoryStorage, scope: &str) -> Arc<dyn DocumentStore> {
    Arc::clone(
        storage
            .for_scope(&Scope::new(scope).unwrap())
            .unwrap()
            .documents(),
    )
}

pub(super) fn store() -> DriverCheckpointer<serde_json::Value> {
    DriverCheckpointer::new(docs(&MemoryStorage::new(), "local"))
}

/// One checkpoint in `thread`, at `step`, chained to `parent`.
pub(super) fn checkpoint(
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
    let store = store();
    store
        .put(checkpoint("t1", "zz-first", None, 1, json!({ "n": 1 })))
        .await
        .unwrap();
    store
        .put(checkpoint(
            "t1",
            "aa-second",
            Some("zz-first"),
            2,
            json!({ "n": 2 }),
        ))
        .await
        .unwrap();

    let latest = store.get("t1", None).await.unwrap().expect("latest");
    assert_eq!(latest.checkpoint_id, "aa-second");
    assert_eq!(latest.state, json!({ "n": 2 }));

    let addressed = store
        .get("t1", Some("zz-first"))
        .await
        .unwrap()
        .expect("zz-first");
    assert_eq!(addressed.state, json!({ "n": 1 }));

    assert!(store.get("t1", Some("nope")).await.unwrap().is_none());
    assert!(store.get("other-thread", None).await.unwrap().is_none());
}

/// Threads must not see each other: a flow run addresses its own thread id,
/// and one flow's checkpoints leaking into another's resume would replay the
/// wrong graph.
#[tokio::test]
async fn threads_are_isolated_and_listed_in_insertion_order() {
    let store = store();
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
    let store = store();
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
    let store = store();
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
    let store = store();
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
    let store = store();
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
async fn scopes_and_namespaces_keep_threads_apart() {
    let storage = MemoryStorage::new();
    let alice = DriverCheckpointer::<serde_json::Value>::new(docs(&storage, "alice"));
    let bob = DriverCheckpointer::<serde_json::Value>::new(docs(&storage, "bob"));
    let other =
        DriverCheckpointer::<serde_json::Value>::with_namespace(docs(&storage, "alice"), "other");
    alice
        .put(checkpoint("t", "c1", None, 1, json!(1)))
        .await
        .unwrap();
    assert!(bob.get("t", None).await.unwrap().is_none());
    assert!(other.get("t", None).await.unwrap().is_none());
    assert_eq!(alice.list_threads().await.unwrap(), vec!["t".to_owned()]);
    assert!(bob.list_threads().await.unwrap().is_empty());
}

#[tokio::test]
async fn scoped_reads_stay_in_their_namespace_when_ids_repeat() {
    let saver = store();
    let child = vec!["sub".to_string()];
    saver
        .put(checkpoint("t", "same", None, 1, json!(1)))
        .await
        .unwrap();
    let mut nested = checkpoint("t", "same", None, 2, json!(2));
    nested.namespace = child.clone();
    saver.put(nested).await.unwrap();
    let root = saver
        .get_scoped("t", Some("same"), &[])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(root.state, json!(1));
    let sub = saver
        .get_scoped("t", Some("same"), &child)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(sub.state, json!(2));
    assert!(
        saver
            .get_scoped("t", None, &["other".to_string()])
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn pending_writes_never_merge_across_lookalike_namespaces() {
    let saver = store();
    saver
        .put(checkpoint("t", "c", None, 1, json!(1)))
        .await
        .unwrap();
    let config = |namespace: Vec<String>| CheckpointConfig {
        thread_id: "t".to_string(),
        checkpoint_id: Some("c".to_string()),
        namespace,
    };
    let split = config(vec!["a".to_string(), "b".to_string()]);
    let joined = config(vec!["a\u{1f}b".to_string()]);
    let write = PendingWrite::data("n", "task", 0, "out", json!("split"));
    saver.put_writes(&split, &[write]).await.unwrap();
    assert!(saver.get_writes(&joined).await.unwrap().is_empty());
    assert_eq!(saver.get_writes(&split).await.unwrap().len(), 1);
}

#[test]
fn keys_are_unambiguous_and_namespaces_encode_injectively() {
    assert_ne!(
        checkpoint_id_key("flows", "a/b", 1),
        checkpoint_id_key("flows", "a", 1)
    );
    assert_ne!(thread_id_key("flows", "a/b"), thread_id_key("flows/a", "b"));
    let ns = |parts: &[&str]| parts.iter().map(|p| (*p).to_string()).collect::<Vec<_>>();
    assert_ne!(subgraph_key(&ns(&["a", "b"])), subgraph_key(&ns(&["a/b"])));
    assert_ne!(subgraph_key(&ns(&["ab"])), subgraph_key(&ns(&["a", "b"])));
    assert_ne!(subgraph_key(&ns(&[""])), subgraph_key(&[]));
}

#[tokio::test]
async fn documents_are_the_v2_graph_layout_under_the_flows_namespace() {
    let storage = MemoryStorage::new();
    let docs = docs(&storage, "local");
    let saver = DriverCheckpointer::<serde_json::Value>::new(Arc::clone(&docs));
    saver
        .put(checkpoint("a/b", "c1", None, 1, json!(1)))
        .await
        .unwrap();
    let row = docs
        .get("graph.checkpoints", &format!("flows/a%2Fb/{:020}", 0))
        .await
        .unwrap()
        .expect("id is Key::of([ns, thread, seq])");
    assert_eq!(row.doc["ns"], "flows");
    assert_eq!(row.doc["thread"], "a/b");
    assert_eq!(row.doc["subgraph"], "");
    assert_eq!(row.doc["checkpoint_id"], "c1");
    assert!(row.doc["checkpoint"].is_object());
    assert_eq!(row.meta.schema, 4);
    let counter = docs
        .get("graph.threads", "flows/a%2Fb")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(counter.doc["next"], 1);
}

#[tokio::test]
async fn a_corrupt_record_is_a_checkpoint_error() {
    let storage = MemoryStorage::new();
    let docs = docs(&storage, "local");
    let saver = DriverCheckpointer::<serde_json::Value>::new(Arc::clone(&docs));
    saver
        .put(checkpoint("t", "c", None, 1, json!(1)))
        .await
        .unwrap();
    let stored = docs
        .query_all(CHECKPOINTS, &Query::all())
        .await
        .unwrap()
        .remove(0);
    let mut doc = stored.doc;
    doc["checkpoint"] = json!("not a checkpoint");
    docs.put(CHECKPOINTS, &stored.id, doc, Precondition::None)
        .await
        .unwrap();
    let error = saver.get("t", None).await.unwrap_err();
    assert!(matches!(error, GraphError::Checkpoint(_)), "{error:?}");
    assert!(
        map_error(StorageError::unavailable("busy"))
            .to_string()
            .contains("busy")
    );
}

/// What the retired file backend's tests held every durable backend to: a
/// fresh handle over the same storage reads what an earlier one wrote.
#[tokio::test]
async fn put_get_list_roundtrip_survives_a_fresh_handle() {
    let storage = MemoryStorage::new();
    let cp = DriverCheckpointer::<serde_json::Value>::new(docs(&storage, "local"));
    cp.put(checkpoint("t1", "c1", None, 1, json!(1)))
        .await
        .unwrap();
    cp.put(checkpoint("t1", "c2", Some("c1"), 2, json!(2)))
        .await
        .unwrap();

    let reopened = DriverCheckpointer::<serde_json::Value>::new(docs(&storage, "local"));
    let latest = reopened.get("t1", None).await.unwrap().unwrap();
    assert_eq!(latest.checkpoint_id, "c2");
    assert_eq!(latest.state, json!(2));
    let first = reopened.get("t1", Some("c1")).await.unwrap().unwrap();
    assert_eq!(first.checkpoint_id, "c1");
    assert!(reopened.get("t1", Some("nope")).await.unwrap().is_none());
    assert!(reopened.get("missing", None).await.unwrap().is_none());

    let list = reopened.list("t1").await.unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].checkpoint_id, "c1");
    assert_eq!(list[1].parent_checkpoint_id.as_deref(), Some("c1"));
    assert_eq!(list[1].step, 2);

    // The tuple convenience composes config + parent from the persisted record.
    let tuple = reopened
        .get_tuple(CheckpointConfig::latest("t1"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(tuple.config.checkpoint_id.as_deref(), Some("c2"));
    assert_eq!(
        tuple.parent_config.unwrap().checkpoint_id.as_deref(),
        Some("c1")
    );
}

#[tokio::test]
async fn thread_ids_with_separators_and_spaces_list_and_delete() {
    let cp = store();
    cp.put(checkpoint("a/b c", "x1", None, 1, json!(1)))
        .await
        .unwrap();
    cp.put(checkpoint("b", "b1", None, 1, json!(1)))
        .await
        .unwrap();

    let mut threads = cp.list_threads().await.unwrap();
    threads.sort();
    assert_eq!(threads, vec!["a/b c".to_string(), "b".to_string()]);

    cp.delete_thread("a/b c").await.unwrap();
    assert_eq!(cp.list_threads().await.unwrap(), vec!["b".to_string()]);
    assert!(cp.get("a/b c", None).await.unwrap().is_none());
    // Deleting a missing thread is a no-op.
    cp.delete_thread("missing").await.unwrap();
}
