//! State-store behaviour: per-namespace scoping, path containment, and the
//! atomic-write guarantee.
//!
//! The last two are why this store is worth sharing rather than rewriting: a
//! key is author-supplied and may contain path separators, and a plain write
//! can leave a key holding a prefix of JSON that no later read recovers from.

use std::sync::Arc;

use serde_json::json;

use super::*;
use crate::caps::StateStore;

#[tokio::test]
async fn state_is_scoped_per_namespace_so_two_workflows_cannot_collide() {
    let root = tempfile::tempdir().unwrap();
    let alpha = FileStateStore::new(root.path(), "workflow:alpha");
    let beta = FileStateStore::new(root.path(), "workflow:beta");

    alpha.store("cursor", json!(1)).await.unwrap();
    beta.store("cursor", json!(2)).await.unwrap();

    assert_eq!(alpha.load("cursor").await.unwrap(), Some(json!(1)));
    assert_eq!(beta.load("cursor").await.unwrap(), Some(json!(2)));
    assert_eq!(alpha.load("missing").await.unwrap(), None);
}

#[tokio::test]
async fn a_state_key_containing_path_separators_cannot_escape_its_directory() {
    let root = tempfile::tempdir().unwrap();
    let store = FileStateStore::new(&root.path().join("state"), "workflow:alpha");

    store.store("../../escaped", json!("x")).await.unwrap();

    // Everything written must live under the namespace directory.
    let escaped = root.path().join("escaped.json");
    assert!(!escaped.exists(), "a key must not choose its own path");
    assert_eq!(
        store.load("../../escaped").await.unwrap(),
        Some(json!("x")),
        "and it must still round-trip"
    );
}

#[tokio::test]
async fn concurrent_writers_of_one_key_never_leave_a_torn_document() {
    // A plain `fs::write` truncates and then fills, so two writers of the same
    // key — or a kill mid-write — can leave a prefix of JSON that no later read
    // can recover from, wedging the key for good. Staging and renaming makes
    // every observable state a whole document.
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(FileStateStore::new(root.path(), "workflow:alpha"));

    // Bodies large enough that a truncating write would be observable in parts.
    let writes: Vec<_> = (0..16)
        .map(|n| {
            let store = store.clone();
            tokio::spawn(async move {
                let body = json!({ "n": n, "pad": "x".repeat(64 * 1024) });
                store.store("cursor", body).await
            })
        })
        .collect();
    for write in writes {
        write.await.unwrap().expect("stores");
    }

    let loaded = store
        .load("cursor")
        .await
        .expect("parses")
        .expect("present");
    assert_eq!(loaded["pad"].as_str().map(str::len), Some(64 * 1024));

    // And no scratch file is left behind: the namespace holds the one document.
    let namespace = std::fs::read_dir(root.path())
        .unwrap()
        .next()
        .expect("the namespace directory")
        .unwrap()
        .path();
    let leftovers: Vec<_> = std::fs::read_dir(namespace)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(leftovers.len(), 1, "{leftovers:?}");
    assert!(leftovers[0].ends_with(".json"), "{leftovers:?}");
}
