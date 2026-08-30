//! Regression coverage for concurrent `save`/`delete` on one store instance.
//!
//! Split out of the sibling `tests` module (already at the repository's
//! 500-line file ceiling) rather than grown into it. What is proven here is
//! narrow but load-bearing: two threads sharing a cloned [`FileWorkflowStore`]
//! — the shape a copilot autosave racing a manual TUI edit takes, both
//! holding `Arc<dyn WorkflowStore>` clones of the same store — must not
//! interleave a save's read-modify-write and lose a revision or a write.

use std::path::Path;
use std::sync::{Arc, Barrier};

use fs2::FileExt;
use serde_json::json;

use super::WorkflowStore;
use super::file::{FileWorkflowStore, definition_state_dir};
use crate::store::types::WorkflowRecord;

/// A store rooted in a temporary directory. Mirrors `tests::store_in`, kept
/// local so this file has no dependency on that module's internals.
fn store_in(root: &Path) -> FileWorkflowStore {
    FileWorkflowStore::new(vec![root.join("workflows")], root.join("runs"))
}

/// A minimal valid document, distinguished only by its `name` so two writers
/// racing on the same id can be told apart afterwards.
fn document(id: &str, name: &str) -> WorkflowRecord {
    let graph = serde_json::from_value(json!({
        "id": id,
        "name": name,
        "nodes": [
            { "id": "t", "kind": "trigger", "name": "start",
              "config": { "trigger_kind": "manual" } },
        ],
        "edges": [],
    }))
    .expect("graph parses");
    WorkflowRecord {
        id: id.to_string(),
        name: name.to_string(),
        description: String::new(),
        enabled: true,
        defaults: Default::default(),
        graph,
        source_path: None,
    }
}

#[test]
fn two_threads_saving_the_same_id_at_once_never_lose_a_revision() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = store_in(root.path());
    store.save(&document("race", "v0")).expect("seed save");

    // Released together so both threads' read-modify-write genuinely overlaps
    // rather than happening to run one after the other by scheduling luck —
    // the failure mode under test is a race, so the test has to race.
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = ["v1", "v2"]
        .into_iter()
        .map(|name| {
            let store = store.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                store.save(&document("race", name)).expect("save")
            })
        })
        .collect();
    for handle in handles {
        handle.join().expect("thread panicked");
    }

    // Three versions existed in total: the seed, and one per racing save. The
    // lock does not decide which save wins the final file — that is still a
    // last-write-wins race, same as any single-writer save — only that each
    // save's own read-then-snapshot-then-write cannot be torn by the other's.
    // Without it, a save could snapshot what the *other* save had already
    // half-written, or overwrite the other's snapshot before it captured
    // anything — either way losing one of the three versions below.
    let revisions = store.list_revisions("race").expect("list revisions");
    assert_eq!(
        revisions.len(),
        2,
        "both saves must have captured the version they superseded: {revisions:?}"
    );
    let current = store.get("race").expect("get").expect("still exists");
    assert!(
        current.name == "v1" || current.name == "v2",
        "the surviving write must be one of the two racing saves, not a torn mix: {}",
        current.name
    );
}

#[test]
fn a_save_racing_a_delete_leaves_the_deletion_recoverable() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = store_in(root.path());
    store.save(&document("race", "v0")).expect("seed save");

    let barrier = Arc::new(Barrier::new(2));
    let save_store = store.clone();
    let save_barrier = barrier.clone();
    let saver = std::thread::spawn(move || {
        save_barrier.wait();
        // Either order is fine — this is a race — but it must not panic or
        // corrupt the store either way.
        let _ = save_store.save(&document("race", "v1"));
    });
    let delete_store = store.clone();
    let deleter = std::thread::spawn(move || {
        barrier.wait();
        let _ = delete_store.delete("race");
    });
    saver.join().expect("save thread panicked");
    deleter.join().expect("delete thread panicked");

    // Whatever order the two actually ran in, `list_revisions` must still
    // reflect every version that existed before whichever write settled last
    // — the same "nothing gets torn" property, just across the two different
    // operations that share the lock.
    let revisions = store.list_revisions("race").expect("list revisions");
    assert!(
        !revisions.is_empty(),
        "at least the seed version must have been captured: {revisions:?}"
    );
}

#[test]
fn failed_definition_publish_does_not_leave_a_revision() {
    let root = tempfile::tempdir().expect("tempdir");
    let lower = root.path().join("defaults");
    let upper = root.path().join("workflows");
    std::fs::create_dir_all(&lower).expect("lower definitions");
    let seed = document("blocked", "v0");
    let seed_store = FileWorkflowStore::new(vec![lower.clone()], root.path().join("seed-runs"));
    seed_store.save(&seed).expect("seed lower definition");

    std::fs::create_dir_all(upper.join("blocked.json")).expect("blocking destination directory");
    let store = FileWorkflowStore::new(vec![lower, upper], root.path().join("runs"));
    assert!(store.save(&document("blocked", "v1")).is_err());
    assert!(
        store
            .list_revisions("blocked")
            .expect("list revisions")
            .is_empty(),
        "a source version that was never superseded must not enter history"
    );
}

#[test]
fn separate_store_instances_use_the_same_definition_lock() {
    let root = tempfile::tempdir().expect("tempdir");
    let first = store_in(root.path());
    let second = store_in(root.path());
    first.save(&document("race", "v0")).expect("seed save");

    let lock_path = definition_state_dir(
        &root.path().join("state/workflows"),
        &[root.path().join("workflows")],
    )
    .join("locks/.race.lock");
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(lock_path)
        .expect("definition lock exists");
    lock.lock_exclusive().expect("claim definition lock");

    let (sent, received) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        sent.send(second.save(&document("race", "v1")))
            .expect("report save");
    });
    assert!(
        received
            .recv_timeout(std::time::Duration::from_millis(100))
            .is_err(),
        "a separate store must wait for the filesystem lock"
    );

    FileExt::unlock(&lock).expect("release definition lock");
    received
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("save completes after unlock")
        .expect("save succeeds");
    writer.join().expect("writer thread");
}

#[test]
fn workspace_scoped_stores_share_the_global_definition_lock() {
    let root = tempfile::tempdir().expect("tempdir");
    let definitions = vec![root.path().join("workflows")];
    let state = root.path().join("state");
    let first = FileWorkflowStore::with_workspace_state(
        definitions.clone(),
        &state,
        &root.path().join("workspace-a"),
    );
    let second = FileWorkflowStore::with_workspace_state(
        definitions,
        &state,
        &root.path().join("workspace-b"),
    );
    first.save(&document("race", "v0")).expect("seed save");

    let lock_path =
        definition_state_dir(&state, &[root.path().join("workflows")]).join("locks/.race.lock");
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(lock_path)
        .expect("global definition lock exists");
    lock.lock_exclusive().expect("claim definition lock");

    let (sent, received) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        sent.send(second.save(&document("race", "v1")))
            .expect("report save");
    });
    assert!(
        received
            .recv_timeout(std::time::Duration::from_millis(100))
            .is_err(),
        "a store for another workspace must wait on the shared definition lock"
    );

    FileExt::unlock(&lock).expect("release definition lock");
    received
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("save completes after unlock")
        .expect("save succeeds");
    writer.join().expect("writer thread");

    let history = first.list_revisions("race").expect("shared history");
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].record.name, "v0");
}

#[test]
fn explicit_stores_derive_locks_from_the_shared_definition_destination() {
    let root = tempfile::tempdir().expect("tempdir");
    let definitions = vec![root.path().join("workflows")];
    let first = FileWorkflowStore::new(definitions.clone(), root.path().join("a/runs"));
    let second = FileWorkflowStore::new(definitions, root.path().join("b/runs"));
    first.save(&document("race", "v0")).expect("seed save");

    let lock_path = definition_state_dir(
        &root.path().join("state/workflows"),
        &[root.path().join("workflows")],
    )
    .join("locks/.race.lock");
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(lock_path)
        .expect("definition-derived lock exists");
    lock.lock_exclusive().expect("claim definition lock");

    let (sent, received) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        sent.send(second.save(&document("race", "v1")))
            .expect("report save");
    });
    assert!(
        received
            .recv_timeout(std::time::Duration::from_millis(100))
            .is_err(),
        "different run roots must not split the shared definition lock"
    );
    FileExt::unlock(&lock).expect("release definition lock");
    received
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("save completes after unlock")
        .expect("save succeeds");
    writer.join().expect("writer thread");
}

#[test]
fn lexical_catalog_aliases_derive_the_same_definition_state() {
    let root = tempfile::tempdir().expect("tempdir");
    let direct = root.path().join("catalog");
    let aliased = root.path().join("missing/../catalog");

    assert_eq!(
        super::file::definition_state_dir(root.path(), &[direct]),
        super::file::definition_state_dir(root.path(), &[aliased])
    );
}

#[cfg(unix)]
#[test]
fn symlinked_catalog_aliases_derive_the_same_definition_state() {
    let root = tempfile::tempdir().expect("tempdir");
    let direct = root.path().join("catalog");
    std::fs::create_dir(&direct).expect("catalog");
    let alias = root.path().join("catalog-link");
    std::os::unix::fs::symlink(&direct, &alias).expect("symlink");

    assert_eq!(
        super::file::definition_state_dir(root.path(), &[direct]),
        super::file::definition_state_dir(root.path(), &[alias])
    );
}

#[test]
fn sibling_definition_catalogs_do_not_share_revision_history() {
    let root = tempfile::tempdir().expect("tempdir");
    let first = FileWorkflowStore::new(
        vec![root.path().join("catalog-a")],
        root.path().join("runs-a"),
    );
    let second = FileWorkflowStore::new(
        vec![root.path().join("catalog-b")],
        root.path().join("runs-b"),
    );
    first.save(&document("same", "a0")).expect("seed first");
    first.save(&document("same", "a1")).expect("edit first");
    second.save(&document("same", "b0")).expect("seed second");

    assert_eq!(
        first.list_revisions("same").expect("first history").len(),
        1
    );
    assert!(
        second
            .list_revisions("same")
            .expect("second history")
            .is_empty()
    );
}

#[cfg(unix)]
#[test]
fn symlinked_catalog_paths_share_definition_state() {
    let root = tempfile::tempdir().expect("tempdir");
    let real = root.path().join("real/workflows");
    std::fs::create_dir_all(&real).expect("real catalog");
    let alias = root.path().join("alias");
    std::os::unix::fs::symlink(root.path().join("real"), &alias).expect("catalog alias");
    let first = FileWorkflowStore::new(vec![real], root.path().join("runs-a"));
    let second = FileWorkflowStore::new(vec![alias.join("workflows")], root.path().join("runs-b"));

    first
        .save(&document("same", "v0"))
        .expect("seed definition");
    second
        .save(&document("same", "v1"))
        .expect("edit through alias");
    assert_eq!(
        first.list_revisions("same").expect("shared history").len(),
        1
    );
}

#[cfg(unix)]
#[test]
fn symlinked_catalog_destination_shares_inferred_state_root() {
    let root = tempfile::tempdir().expect("tempdir");
    let real = root.path().join("real/catalog");
    let alias_parent = root.path().join("alias");
    std::fs::create_dir_all(&real).expect("real catalog");
    std::fs::create_dir(&alias_parent).expect("alias parent");
    let alias = alias_parent.join("catalog-link");
    std::os::unix::fs::symlink(&real, &alias).expect("catalog symlink");
    let first = FileWorkflowStore::new(vec![real], root.path().join("runs-a"));
    let second = FileWorkflowStore::new(vec![alias], root.path().join("runs-b"));
    first
        .save(&document("same", "v0"))
        .expect("seed definition");
    second
        .save(&document("same", "v1"))
        .expect("edit through alias");
    assert_eq!(
        first.list_revisions("same").expect("shared history").len(),
        1
    );
}

#[cfg(unix)]
#[test]
fn parent_after_symlink_uses_filesystem_catalog_identity() {
    let root = tempfile::tempdir().expect("tempdir");
    let other = root.path().join("other");
    std::fs::create_dir_all(other.join("inner")).expect("symlink target");
    std::fs::create_dir(other.join("catalog")).expect("real catalog");
    std::os::unix::fs::symlink(other.join("inner"), root.path().join("link"))
        .expect("directory symlink");
    let aliased = root.path().join("link/../catalog");
    assert_eq!(
        definition_state_dir(root.path(), &[aliased]),
        definition_state_dir(root.path(), &[other.join("catalog")])
    );
}

#[test]
fn separate_store_instances_serialize_proposal_decisions() {
    let root = tempfile::tempdir().expect("tempdir");
    let first = store_in(root.path());
    let second = store_in(root.path());
    let first_claim = first
        .lock_proposal_decision("race")
        .expect("claim proposal decisions");

    let (sent, received) = std::sync::mpsc::channel();
    let contender = std::thread::spawn(move || {
        let claim = second.lock_proposal_decision("race");
        sent.send(claim.map(drop)).expect("report decision claim");
    });
    assert!(
        received
            .recv_timeout(std::time::Duration::from_millis(100))
            .is_err(),
        "another store must wait before deciding the same workflow"
    );

    drop(first_claim);
    received
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("claim completes after unlock")
        .expect("claim succeeds");
    contender.join().expect("contender thread");
}
