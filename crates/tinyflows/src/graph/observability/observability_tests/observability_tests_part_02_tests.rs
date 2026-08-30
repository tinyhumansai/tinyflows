
#[tokio::test]
async fn status_store_cap_evicts_oldest_terminal_first() {
    let store = InMemoryGraphStatusStore::new().with_max_runs(2);
    store
        .put_status(status_on_thread("r-live", "t", ExecutionStatus::Running))
        .await
        .unwrap();
    store
        .put_status(status_on_thread("r-done", "t", ExecutionStatus::Completed))
        .await
        .unwrap();
    // Third run exceeds the cap: the terminal `r-done` goes first even though
    // `r-live` is older.
    store
        .put_status(status_on_thread("r-new", "t", ExecutionStatus::Running))
        .await
        .unwrap();

    assert_eq!(store.len(), 2);
    assert!(store.get_status("r-done").await.unwrap().is_none());
    assert!(store.get_status("r-live").await.unwrap().is_some());
    assert!(store.get_status("r-new").await.unwrap().is_some());

    // The thread index no longer serves the evicted run.
    let by_thread = store.list_by_thread("t").await.unwrap();
    assert_eq!(by_thread.len(), 2);
    assert!(by_thread.iter().all(|s| s.run_id.as_str() != "r-done"));
}

#[tokio::test]
async fn status_store_cap_falls_back_to_oldest_live_run() {
    let store = InMemoryGraphStatusStore::new().with_max_runs(2);
    for id in ["r-1", "r-2", "r-3"] {
        store
            .put_status(status_on_thread(id, "t", ExecutionStatus::Running))
            .await
            .unwrap();
    }
    // No terminal run to prefer, so the oldest live run is evicted.
    assert_eq!(store.len(), 2);
    assert!(store.get_status("r-1").await.unwrap().is_none());
    assert!(store.get_status("r-2").await.unwrap().is_some());
    assert!(store.get_status("r-3").await.unwrap().is_some());
    assert_eq!(store.list_by_thread("t").await.unwrap().len(), 2);
}

#[tokio::test]
async fn status_store_overwrite_never_evicts() {
    let store = InMemoryGraphStatusStore::new().with_max_runs(2);
    store
        .put_status(status_on_thread("r-1", "t", ExecutionStatus::Running))
        .await
        .unwrap();
    store
        .put_status(status_on_thread("r-2", "t", ExecutionStatus::Running))
        .await
        .unwrap();
    // Updating an existing run at capacity is not an insertion.
    store
        .put_status(status_on_thread("r-1", "t", ExecutionStatus::Completed))
        .await
        .unwrap();
    assert_eq!(store.len(), 2);
    assert!(store.get_status("r-1").await.unwrap().is_some());
    assert!(store.get_status("r-2").await.unwrap().is_some());
}
