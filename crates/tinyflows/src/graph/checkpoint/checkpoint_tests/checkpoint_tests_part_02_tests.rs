
#[tokio::test]
async fn prune_keeps_a_window_per_namespace() {
    let cp = InMemoryCheckpointer::<i32>::new();
    // An embedded subgraph writes under the parent's thread but its own
    // namespace, interleaved with the parent's records. The two lineages are
    // disjoint — no parent record ever references a child id — so a
    // thread-wide recency window would delete the child lineage outright and
    // leave the thread unresumable.
    let sub = |id: &str, parent: Option<&str>, step: usize| {
        let mut c = checkpoint("t", id, parent, step);
        c.namespace = vec!["sub".to_string()];
        c
    };
    cp.put(checkpoint("t", "p1", None, 1)).await.unwrap();
    cp.put(sub("s1", None, 1)).await.unwrap();
    cp.put(sub("s2", Some("s1"), 2)).await.unwrap();
    cp.put(checkpoint("t", "p2", Some("p1"), 2)).await.unwrap();

    // Keep the last 1 of each namespace plus its ancestors: {p2, p1} and
    // {s2, s1} — nothing is deleted, and the subgraph stays resolvable.
    let removed = cp.prune("t", 1).await.unwrap();
    assert_eq!(removed, 0);
    let child = cp
        .get_scoped("t", None, &["sub".to_string()])
        .await
        .unwrap()
        .expect("the subgraph namespace must stay resumable after prune");
    assert_eq!(child.checkpoint_id, "s2");
}

// ---- File-backed checkpointer ---------------------------------------------

mod file_backend {
    use super::checkpoint;
    use crate::graph::checkpoint::{CheckpointConfig, Checkpointer, FileCheckpointer};
    use std::path::PathBuf;

    /// A unique-per-test temp dir derived from the test name + pid (no clock).
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(test_name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "tinyflows-graph-ckpt-{}-{}",
                test_name,
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            Self(dir)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn put_get_list_roundtrip_survives_a_fresh_handle() {
        let tmp = TempDir::new("roundtrip");
        let cp = FileCheckpointer::<i32>::new(tmp.path());

        cp.put(checkpoint("t1", "c1", None, 1)).await.unwrap();
        cp.put(checkpoint("t1", "c2", Some("c1"), 2)).await.unwrap();

        // A brand-new handle over the same dir reads what was persisted —
        // proving the records hit disk rather than living in memory.
        let reopened = FileCheckpointer::<i32>::new(tmp.path());
        let latest = reopened.get("t1", None).await.unwrap().unwrap();
        assert_eq!(latest.checkpoint_id, "c2");
        assert_eq!(latest.state, 2);

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
    async fn list_threads_and_delete_thread_track_files() {
        let tmp = TempDir::new("threads");
        // A thread id with separators/spaces exercises filename escaping.
        let cp = FileCheckpointer::<i32>::new(tmp.path());
        cp.put(checkpoint("a/b c", "x1", None, 1)).await.unwrap();
        cp.put(checkpoint("b", "b1", None, 1)).await.unwrap();

        let mut threads = cp.list_threads().await.unwrap();
        threads.sort();
        assert_eq!(threads, vec!["a/b c".to_string(), "b".to_string()]);

        cp.delete_thread("a/b c").await.unwrap();
        assert_eq!(cp.list_threads().await.unwrap(), vec!["b".to_string()]);
        assert!(cp.get("a/b c", None).await.unwrap().is_none());
        // Deleting a missing thread is a no-op.
        cp.delete_thread("missing").await.unwrap();
    }

    #[tokio::test]
    async fn legacy_uppercase_thread_files_remain_readable_and_copyable() {
        let tmp = TempDir::new("legacy-uppercase");
        let cp = FileCheckpointer::<i32>::new(tmp.path());
        cp.put(checkpoint("Run", "c1", None, 1)).await.unwrap();

        // Simulate the pre-upgrade filename scheme, which kept uppercase
        // letters unescaped (`Run.jsonl` rather than `%52un.jsonl`).
        std::fs::rename(tmp.path().join("%52un.jsonl"), tmp.path().join("Run.jsonl")).unwrap();

        assert_eq!(cp.get("Run", None).await.unwrap().unwrap().state, 1);
        cp.copy_thread("Run", "copy").await.unwrap();
        assert_eq!(cp.get("copy", None).await.unwrap().unwrap().state, 1);
        cp.delete_thread("Run").await.unwrap();
        assert!(cp.get("Run", None).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn prune_rewrites_the_thread_file() {
        let tmp = TempDir::new("prune");
        let cp = FileCheckpointer::<i32>::new(tmp.path());
        cp.put(checkpoint("t", "c1", None, 1)).await.unwrap();
        cp.put(checkpoint("t", "b2", Some("c1"), 2)).await.unwrap();
        cp.put(checkpoint("t", "m2", Some("c1"), 3)).await.unwrap();
        cp.put(checkpoint("t", "m3", Some("m2"), 4)).await.unwrap();

        // Keep last 1 (m3) + its ancestors (m2, c1); the dead fork b2 is pruned.
        let removed = cp.prune("t", 1).await.unwrap();
        assert_eq!(removed, 1);
        let remaining: std::collections::HashSet<String> = cp
            .list("t")
            .await
            .unwrap()
            .into_iter()
            .map(|m| m.checkpoint_id)
            .collect();
        assert_eq!(
            remaining,
            ["c1", "m2", "m3"].iter().map(|s| s.to_string()).collect()
        );

        // Deleting everything removes the underlying file, so the thread drops
        // out of the listing.
        cp.delete_checkpoints("t", &["c1".into(), "m2".into(), "m3".into()])
            .await
            .unwrap();
        assert!(cp.list_threads().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn copy_thread_rewrites_thread_ids_on_disk() {
        let tmp = TempDir::new("copy");
        let cp = FileCheckpointer::<i32>::new(tmp.path());
        cp.put(checkpoint("src", "c1", None, 1)).await.unwrap();
        cp.put(checkpoint("src", "c2", Some("c1"), 2))
            .await
            .unwrap();
        cp.put(checkpoint("src", "c3", Some("c2"), 3))
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
        assert_eq!(tip.state, 3);
    }

    #[tokio::test]
    async fn get_thread_reads_the_file_once_in_order() {
        let tmp = TempDir::new("get-thread");
        let cp = FileCheckpointer::<i32>::new(tmp.path());
        cp.put(checkpoint("t", "c1", None, 1)).await.unwrap();
        cp.put(checkpoint("t", "c2", Some("c1"), 2)).await.unwrap();

        let records = cp.get_thread("t").await.unwrap();
        let ids: Vec<&str> = records.iter().map(|c| c.checkpoint_id.as_str()).collect();
        assert_eq!(ids, vec!["c1", "c2"]);
        assert_eq!(records[1].state, 2);
        assert!(cp.get_thread("missing").await.unwrap().is_empty());
    }
}
