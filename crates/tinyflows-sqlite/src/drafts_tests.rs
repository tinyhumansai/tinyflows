use super::*;
use serde_json::json;
use tempfile::TempDir;

/// The catalog directory a test writes drafts into. Nested under the
/// `TempDir` rather than being the `TempDir` itself, so the code under test
/// has to create it — a store that only works against a directory somebody
/// else made is a store that fails on first run.
fn test_dir(tmp: &TempDir) -> PathBuf {
    tmp.path().join("workspace").join("flows")
}

fn sample_graph() -> Value {
    json!({ "nodes": [ { "id": "t", "kind": "trigger", "name": "Manual" } ], "edges": [] })
}

#[test]
fn create_get_roundtrip() {
    let tmp = TempDir::new().unwrap();
    let dir = test_dir(&tmp);
    let draft = create_draft(
        &dir,
        None,
        "My draft".into(),
        sample_graph(),
        DraftOrigin::Chat,
    )
    .unwrap();
    let loaded = get_draft(&dir, &draft.id).unwrap().unwrap();
    assert_eq!(loaded, draft);
    assert_eq!(loaded.name, "My draft");
    assert_eq!(loaded.origin, DraftOrigin::Chat);
    assert!(loaded.flow_id.is_none());
}

#[test]
fn get_missing_is_none() {
    let tmp = TempDir::new().unwrap();
    let dir = test_dir(&tmp);
    assert!(get_draft(&dir, "does-not-exist").unwrap().is_none());
}

#[test]
fn update_patches_fields_and_bumps_updated_at() {
    let tmp = TempDir::new().unwrap();
    let dir = test_dir(&tmp);
    let draft = create_draft(
        &dir,
        None,
        "Old".into(),
        sample_graph(),
        DraftOrigin::Canvas,
    )
    .unwrap();
    let updated = update_draft(
        &dir,
        &draft.id,
        Some("New name".into()),
        Some(json!({ "nodes": [], "edges": [] })),
        Some(Some("flow-42".into())),
    )
    .unwrap();
    assert_eq!(updated.name, "New name");
    assert_eq!(updated.flow_id.as_deref(), Some("flow-42"));
    assert_eq!(updated.graph["nodes"].as_array().unwrap().len(), 0);
    assert!(updated.updated_at >= draft.updated_at);
    assert_eq!(updated.created_at, draft.created_at);
}

#[test]
fn list_returns_newest_first_and_delete_removes() {
    let tmp = TempDir::new().unwrap();
    let dir = test_dir(&tmp);
    let a = create_draft(&dir, None, "A".into(), sample_graph(), DraftOrigin::Chat).unwrap();
    // Bump a second draft's updated_at by updating it after creation.
    let b = create_draft(&dir, None, "B".into(), sample_graph(), DraftOrigin::Chat).unwrap();
    let b = update_draft(&dir, &b.id, Some("B2".into()), None, None).unwrap();

    let list = list_drafts(&dir).unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].id, b.id, "newest-updated first");

    assert!(delete_draft(&dir, &a.id).unwrap());
    assert!(
        !delete_draft(&dir, &a.id).unwrap(),
        "second delete is a no-op"
    );
    assert_eq!(list_drafts(&dir).unwrap().len(), 1);
}

#[test]
fn list_on_missing_dir_is_empty() {
    let tmp = TempDir::new().unwrap();
    let dir = test_dir(&tmp);
    assert!(list_drafts(&dir).unwrap().is_empty());
}

#[test]
fn rejects_path_traversal_ids() {
    let tmp = TempDir::new().unwrap();
    let dir = test_dir(&tmp);
    assert!(get_draft(&dir, "../secret").is_err());
    assert!(draft_path(&dir, "a/b").is_err());
    assert!(draft_path(&dir, "..").is_err());
    assert!(draft_path(&dir, "ok-123_ID").is_ok());
}

/// Regression: two concurrent `update_draft` calls patching different fields
/// of the SAME draft must not lose either patch. Without the per-path lock,
/// both calls can read the same on-disk version, apply their own field, and
/// whichever write lands last silently discards the other's change; with the
/// lock, the second call's read always sees the first call's completed
/// write, so both patches survive regardless of thread scheduling.
#[test]
fn concurrent_updates_to_different_fields_do_not_lose_either_patch() {
    let tmp = TempDir::new().unwrap();
    let dir = test_dir(&tmp);
    let draft = create_draft(
        &dir,
        None,
        "original".into(),
        sample_graph(),
        DraftOrigin::Chat,
    )
    .unwrap();

    let dir_a = dir.clone();
    let dir_b = dir.clone();
    let id_a = draft.id.clone();
    let id_b = draft.id.clone();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let barrier_a = barrier.clone();
    let barrier_b = barrier.clone();

    let handle_a = std::thread::spawn(move || {
        barrier_a.wait();
        update_draft(&dir_a, &id_a, Some("renamed".into()), None, None).unwrap();
    });
    let handle_b = std::thread::spawn(move || {
        barrier_b.wait();
        update_draft(&dir_b, &id_b, None, Some(json!({ "patched": true })), None).unwrap();
    });
    handle_a.join().unwrap();
    handle_b.join().unwrap();

    let final_draft = get_draft(&dir, &draft.id).unwrap().expect("draft present");
    assert_eq!(final_draft.name, "renamed", "name patch must not be lost");
    assert_eq!(
        final_draft.graph,
        json!({ "patched": true }),
        "graph patch must not be lost"
    );
}

#[test]
fn inactive_draft_locks_are_pruned_instead_of_accumulating() {
    let root = PathBuf::from(format!("/draft-lock-prune-{}", Uuid::new_v4()));
    for index in 0..32 {
        let lock = lock_for(&root.join(format!("{index}.json")));
        drop(lock);
    }
    let final_lock = lock_for(&root.join("final.json"));
    let registry = DRAFT_LOCKS.get().expect("lock registry");
    assert_eq!(
        registry
            .lock()
            .unwrap()
            .keys()
            .filter(|path| path.starts_with(&root))
            .count(),
        1
    );
    drop(final_lock);
}
