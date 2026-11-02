//! Checkpointer lineage and insertion-order behavior.

use super::tests::{checkpoint, store};
use serde_json::json;
use tinyflows::graph::checkpoint::Checkpointer;

#[tokio::test]
async fn copy_thread_carries_the_lineage_under_the_new_thread_id() {
    let cp = store();
    cp.put(checkpoint("src", "c1", None, 1, json!(1)))
        .await
        .unwrap();
    cp.put(checkpoint("src", "c2", Some("c1"), 2, json!(2)))
        .await
        .unwrap();
    cp.put(checkpoint("src", "c3", Some("c2"), 3, json!(3)))
        .await
        .unwrap();

    cp.copy_thread("src", "dst").await.unwrap();

    // Source untouched.
    assert_eq!(cp.list("src").await.unwrap().len(), 3);

    // Target carries the same lineage under the new thread id.
    let copied = cp.list("dst").await.unwrap();
    assert_eq!(copied.len(), 3);
    assert!(copied.iter().all(|m| m.thread_id == "dst"));
    assert_eq!(copied[2].checkpoint_id, "c3");
    assert_eq!(copied[2].parent_checkpoint_id.as_deref(), Some("c2"));
    let tip = cp.get("dst", None).await.unwrap().unwrap();
    assert_eq!(tip.thread_id, "dst");
    assert_eq!(tip.state, json!(3));
}

#[tokio::test]
async fn get_thread_reads_in_insertion_order() {
    let cp = store();
    cp.put(checkpoint("t", "c1", None, 1, json!(1)))
        .await
        .unwrap();
    cp.put(checkpoint("t", "c2", Some("c1"), 2, json!(2)))
        .await
        .unwrap();

    let records = cp.get_thread("t").await.unwrap();
    let ids: Vec<&str> = records.iter().map(|c| c.checkpoint_id.as_str()).collect();
    assert_eq!(ids, vec!["c1", "c2"]);
    assert_eq!(records[1].state, json!(2));
    assert!(cp.get_thread("missing").await.unwrap().is_empty());
}
