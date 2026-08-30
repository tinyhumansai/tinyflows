
#[tokio::test]
async fn put_get_list_roundtrip() {
    let cp = InMemoryCheckpointer::<i32>::new();

    cp.put(checkpoint("t1", "c1", None, 1)).await.unwrap();
    cp.put(checkpoint("t1", "c2", Some("c1"), 2)).await.unwrap();

    // latest
    let latest = cp.get("t1", None).await.unwrap().unwrap();
    assert_eq!(latest.checkpoint_id, "c2");
    assert_eq!(latest.state, 2);

    // specific
    let first = cp.get("t1", Some("c1")).await.unwrap().unwrap();
    assert_eq!(first.checkpoint_id, "c1");

    // missing thread
    assert!(cp.get("other", None).await.unwrap().is_none());

    // list
    let list = cp.list("t1").await.unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].checkpoint_id, "c1");
    assert_eq!(list[1].parent_checkpoint_id.as_deref(), Some("c1"));
    assert_eq!(list[1].step, 2);
}

#[test]
fn legacy_checkpoint_json_without_new_fields_still_loads() {
    // Back-compat: a checkpoint serialized before `pending_activations` /
    // `barrier_arrivals` existed must deserialize, defaulting those fields so
    // resume falls back to `next_nodes` and an empty barrier set.
    let legacy = json!({
        "thread_id": "t",
        "checkpoint_id": "c1",
        "run_id": null,
        "parent_checkpoint_id": null,
        "namespace": [],
        "state": 7,
        "next_nodes": ["a", "b"],
        "completed_tasks": [],
        "pending_writes": [],
        "interrupts": [],
        "metadata": { "source": "loop", "step": 1 }
    });
    let cp: Checkpoint<i32> = serde_json::from_value(legacy).unwrap();
    assert_eq!(cp.state, 7);
    assert_eq!(cp.next_nodes.len(), 2);
    assert!(cp.pending_activations.is_none());
    assert!(cp.barrier_arrivals.is_empty());
}

#[test]
fn pending_activation_send_arg_roundtrips() {
    let cp = Checkpoint {
        thread_id: "t".into(),
        checkpoint_id: "c1".into(),
        run_id: None,
        parent_checkpoint_id: None,
        namespace: vec![],
        state: 1i32,
        next_nodes: vec![NodeId::from("w")],
        completed_tasks: vec![],
        pending_writes: vec![],
        interrupts: vec![],
        pending_activations: Some(vec![super::PendingActivation {
            node: NodeId::from("w"),
            send_arg: Some(json!({ "item": 42 })),
            task_id: "1:0:w".to_string(),
        }]),
        barrier_arrivals: vec![super::BarrierArrivals {
            node: NodeId::from("join"),
            arrived: vec![NodeId::from("p1")],
        }],
        metadata: json!({ "source": "loop", "step": 1 }),
    };
    let round: Checkpoint<i32> =
        serde_json::from_str(&serde_json::to_string(&cp).unwrap()).unwrap();
    let pa = round.pending_activations.unwrap();
    assert_eq!(pa[0].send_arg, Some(json!({ "item": 42 })));
    assert_eq!(round.barrier_arrivals[0].arrived, vec![NodeId::from("p1")]);
}

#[tokio::test]
async fn clones_share_storage() {
    let cp = InMemoryCheckpointer::<i32>::new();
    let cp2 = cp.clone();
    cp.put(checkpoint("t", "c1", None, 1)).await.unwrap();
    assert_eq!(cp2.count("t"), 1);
}

#[test]
fn checkpoint_source_roundtrips_string_and_display() {
    for src in [
        CheckpointSource::Input,
        CheckpointSource::Loop,
        CheckpointSource::Update,
        CheckpointSource::Fork,
    ] {
        let s = src.to_string();
        assert_eq!(s, src.as_str());
        assert_eq!(CheckpointSource::parse(&s), Some(src));
        // serde wire form matches the Display/string form.
        let json = serde_json::to_string(&src).unwrap();
        assert_eq!(json, format!("\"{s}\""));
    }
    assert_eq!(CheckpointSource::parse("nope"), None);
}

#[test]
fn durability_mode_defaults_to_sync() {
    assert_eq!(DurabilityMode::default(), DurabilityMode::Sync);
}

#[tokio::test]
async fn list_metadata_parses_source_enum() {
    let cp = InMemoryCheckpointer::<i32>::new();
    let mut c = checkpoint("t1", "c1", None, 0);
    c.metadata = json!({ "source": "input", "step": 0 });
    cp.put(c).await.unwrap();
    // Unknown/missing source falls back to `loop`.
    let mut c2 = checkpoint("t1", "c2", Some("c1"), 1);
    c2.metadata = json!({ "step": 1 });
    cp.put(c2).await.unwrap();

    let list = cp.list("t1").await.unwrap();
    assert_eq!(list[0].source, CheckpointSource::Input);
    assert_eq!(list[1].source, CheckpointSource::Loop);
}

#[tokio::test]
async fn get_tuple_composes_config_and_parent() {
    let cp = InMemoryCheckpointer::<i32>::new();
    cp.put(checkpoint("t1", "c1", None, 1)).await.unwrap();
    cp.put(checkpoint("t1", "c2", Some("c1"), 2)).await.unwrap();

    // Latest tuple resolves the concrete id and its parent config.
    let tuple = cp
        .get_tuple(CheckpointConfig::latest("t1"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(tuple.config.checkpoint_id.as_deref(), Some("c2"));
    assert_eq!(tuple.checkpoint.checkpoint_id, "c2");
    let parent = tuple.parent_config.unwrap();
    assert_eq!(parent.checkpoint_id.as_deref(), Some("c1"));
    assert_eq!(parent.thread_id, "t1");

    // The root checkpoint has no parent config.
    let root = cp
        .get_tuple(CheckpointConfig {
            thread_id: "t1".to_string(),
            checkpoint_id: Some("c1".to_string()),
            namespace: vec![],
        })
        .await
        .unwrap()
        .unwrap();
    assert!(root.parent_config.is_none());

    // Missing thread yields no tuple.
    assert!(
        cp.get_tuple(CheckpointConfig::latest("missing"))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn list_threads_and_delete_thread() {
    let cp = InMemoryCheckpointer::<i32>::new();
    cp.put(checkpoint("a", "a1", None, 1)).await.unwrap();
    cp.put(checkpoint("b", "b1", None, 1)).await.unwrap();

    let mut threads = cp.list_threads().await.unwrap();
    threads.sort();
    assert_eq!(threads, vec!["a".to_string(), "b".to_string()]);

    cp.delete_thread("a").await.unwrap();
    assert_eq!(cp.list_threads().await.unwrap(), vec!["b".to_string()]);
    assert!(cp.get("a", None).await.unwrap().is_none());
    // Deleting a missing thread is a no-op.
    cp.delete_thread("missing").await.unwrap();
}

#[tokio::test]
async fn delete_by_run_removes_only_matching_run() {
    let cp = InMemoryCheckpointer::<i32>::new();
    let mut c1 = checkpoint("t", "c1", None, 1);
    c1.run_id = Some("run-1".to_string());
    let mut c2 = checkpoint("t", "c2", Some("c1"), 2);
    c2.run_id = Some("run-2".to_string());
    let mut c3 = checkpoint("t", "c3", Some("c2"), 3);
    c3.run_id = Some("run-2".to_string());
    cp.put(c1).await.unwrap();
    cp.put(c2).await.unwrap();
    cp.put(c3).await.unwrap();

    let removed = cp.delete_by_run("t", "run-2").await.unwrap();
    assert_eq!(removed, 2);
    let remaining: Vec<String> = cp
        .list("t")
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.checkpoint_id)
        .collect();
    assert_eq!(remaining, vec!["c1".to_string()]);
    // Records with no run id are never matched.
    assert_eq!(cp.delete_by_run("t", "run-1").await.unwrap(), 1);
}

#[tokio::test]
async fn copy_thread_preserves_lineage() {
    let cp = InMemoryCheckpointer::<i32>::new();
    cp.put(checkpoint("src", "c1", None, 1)).await.unwrap();
    cp.put(checkpoint("src", "c2", Some("c1"), 2))
        .await
        .unwrap();
    cp.put(checkpoint("src", "c3", Some("c2"), 3))
        .await
        .unwrap();

    cp.copy_thread("src", "dst").await.unwrap();

    // The source is untouched.
    assert_eq!(cp.count("src"), 3);

    // The target carries the same records (ids + parent chain) under a new
    // thread id, so time-travel walks the copied thread identically.
    let copied = cp.list("dst").await.unwrap();
    assert_eq!(copied.len(), 3);
    assert!(copied.iter().all(|m| m.thread_id == "dst"));
    assert_eq!(copied[0].checkpoint_id, "c1");
    assert_eq!(copied[0].parent_checkpoint_id, None);
    assert_eq!(copied[2].checkpoint_id, "c3");
    assert_eq!(copied[2].parent_checkpoint_id.as_deref(), Some("c2"));

    // The copied checkpoint's state and addressing are intact.
    let tip = cp.get("dst", None).await.unwrap().unwrap();
    assert_eq!(tip.thread_id, "dst");
    assert_eq!(tip.state, 3);
}

#[tokio::test]
async fn get_thread_returns_full_records_in_listing_order() {
    let cp = InMemoryCheckpointer::<i32>::new();
    cp.put(checkpoint("t", "c1", None, 1)).await.unwrap();
    cp.put(checkpoint("t", "c2", Some("c1"), 2)).await.unwrap();
    cp.put(checkpoint("t", "c3", Some("c2"), 3)).await.unwrap();

    let records = cp.get_thread("t").await.unwrap();
    assert_eq!(records.len(), 3);
    // Full checkpoints (state included), in insertion order.
    let ids: Vec<&str> = records.iter().map(|c| c.checkpoint_id.as_str()).collect();
    assert_eq!(ids, vec!["c1", "c2", "c3"]);
    assert_eq!(records[2].state, 3);
    assert_eq!(records[2].parent_checkpoint_id.as_deref(), Some("c2"));

    // Unknown threads read as empty.
    assert!(cp.get_thread("missing").await.unwrap().is_empty());
}

#[tokio::test]
async fn prune_keeps_window_and_full_ancestor_chain() {
    let cp = InMemoryCheckpointer::<i32>::new();
    // Linear lineage c1 <- c2 <- c3 <- c4 <- c5.
    cp.put(checkpoint("t", "c1", None, 1)).await.unwrap();
    cp.put(checkpoint("t", "c2", Some("c1"), 2)).await.unwrap();
    cp.put(checkpoint("t", "c3", Some("c2"), 3)).await.unwrap();
    cp.put(checkpoint("t", "c4", Some("c3"), 4)).await.unwrap();
    cp.put(checkpoint("t", "c5", Some("c4"), 5)).await.unwrap();

    // Keep the last 2 (c4, c5). Their ancestor chain (c3, c2, c1) must be
    // retained too — a linear lineage protects everything, deleting nothing.
    let removed = cp.prune("t", 2).await.unwrap();
    assert_eq!(removed, 0);
    assert_eq!(cp.count("t"), 5);
}

#[tokio::test]
async fn prune_drops_off_lineage_branches() {
    let cp = InMemoryCheckpointer::<i32>::new();
    // c1 is the shared root. A dead fork b2 branches off c1 and is never an
    // ancestor of the kept tip; the live spine is c1 <- m2 <- m3.
    cp.put(checkpoint("t", "c1", None, 1)).await.unwrap();
    cp.put(checkpoint("t", "b2", Some("c1"), 2)).await.unwrap();
    cp.put(checkpoint("t", "m2", Some("c1"), 3)).await.unwrap();
    cp.put(checkpoint("t", "m3", Some("m2"), 4)).await.unwrap();

    // Keep the last 1 (m3). Protected = {m3} ∪ ancestors {m2, c1}. The dead
    // fork b2 is not an ancestor of anything kept, so it is pruned, but the
    // ancestor chain a kept delta depends on (m2, c1) survives.
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
}

#[tokio::test]
async fn prune_zero_keeps_latest_and_its_chain() {
    let cp = InMemoryCheckpointer::<i32>::new();
    cp.put(checkpoint("t", "c1", None, 1)).await.unwrap();
    cp.put(checkpoint("t", "c2", Some("c1"), 2)).await.unwrap();

    // keep_last == 0 is clamped to 1: the latest checkpoint (and its ancestor
    // chain) is always retained so the thread stays resumable.
    let removed = cp.prune("t", 0).await.unwrap();
    assert_eq!(removed, 0);
    assert_eq!(cp.count("t"), 2);
}
