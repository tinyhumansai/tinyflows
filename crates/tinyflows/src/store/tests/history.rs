//! Revision snapshotting, undo, and rollback: saving over a workflow
//! snapshots what it replaced, undo restores (and is itself undoable), a named
//! rollback restores exactly that revision, and history never leaks into the
//! ordinary workflow listing.

use super::*;

#[test]
fn a_workflow_saved_for_the_first_time_has_no_history_to_go_back_to() {
    let root = tempfile::tempdir().unwrap();
    let store = store_in(root.path());

    store
        .save(&parse_workflow(&valid_document("greet"), "greet").unwrap())
        .unwrap();

    // Nothing was replaced, so nothing was superseded.
    assert!(store.list_revisions("greet").unwrap().is_empty());
    assert!(undo_last(&store, "greet").unwrap().is_none());
}

#[test]
fn saving_over_a_workflow_snapshots_the_version_it_replaced() {
    let root = tempfile::tempdir().unwrap();
    let store = store_in(root.path());
    let mut record = parse_workflow(&valid_document("greet"), "greet").unwrap();
    store.save(&record).unwrap();

    record.description = "rewritten by the copilot".into();
    store.save(&record).unwrap();

    let history = store.list_revisions("greet").unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].record.description, "says hello");
}

#[test]
fn legacy_and_new_revisions_are_listed_together_after_an_edit() {
    let root = tempfile::tempdir().unwrap();
    let store = store_in(root.path());
    let mut record = parse_workflow(&valid_document("greet"), "greet").unwrap();
    record.description = "legacy version".into();
    store.save(&record).unwrap();
    record.description = "current at upgrade".into();
    store.save(&record).unwrap();

    let legacy_dir = root.path().join("workflows/.revisions/greet");
    std::fs::create_dir_all(legacy_dir.parent().unwrap()).unwrap();
    std::fs::rename(
        super::super::file::definition_state_dir(
            &root.path().join("state/workflows"),
            &[root.path().join("workflows")],
        )
        .join("revisions/greet"),
        &legacy_dir,
    )
    .unwrap();

    record.description = "post-upgrade edit".into();
    store.save(&record).unwrap();

    let history = store.list_revisions("greet").unwrap();
    let descriptions: Vec<_> = history
        .iter()
        .map(|revision| revision.record.description.as_str())
        .collect();
    assert_eq!(descriptions, ["current at upgrade", "legacy version"]);

    let legacy = history
        .iter()
        .find(|revision| revision.record.description == "legacy version")
        .expect("legacy revision remains addressable");
    let restored = rollback(&store, "greet", &legacy.id).expect("legacy rollback");
    assert_eq!(restored.description, "legacy version");
    assert_eq!(
        store.get("greet").unwrap().unwrap().description,
        "legacy version"
    );
}

#[test]
fn workspace_scoped_stores_share_definition_revision_history() {
    let root = tempfile::tempdir().unwrap();
    let definitions = vec![root.path().join("workflows")];
    let state = root.path().join("state/workflows");
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
    let mut record = parse_workflow(&valid_document("greet"), "greet").unwrap();
    first.save(&record).unwrap();
    record.description = "edited from workspace a".into();
    first.save(&record).unwrap();

    let history = second.list_revisions("greet").unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].record.description, "says hello");
}

#[test]
fn sibling_definition_catalogs_do_not_share_revision_history() {
    let root = tempfile::tempdir().unwrap();
    let first = FileWorkflowStore::new(
        vec![root.path().join("catalog-a")],
        root.path().join("runs-a"),
    );
    let second = FileWorkflowStore::new(
        vec![root.path().join("catalog-b")],
        root.path().join("runs-b"),
    );

    let mut first_record = parse_workflow(&valid_document("greet"), "greet").unwrap();
    first.save(&first_record).unwrap();
    first_record.description = "catalog a edit".into();
    first.save(&first_record).unwrap();

    let mut second_record = parse_workflow(&valid_document("greet"), "greet").unwrap();
    second_record.description = "catalog b original".into();
    second.save(&second_record).unwrap();
    second_record.description = "catalog b edit".into();
    second.save(&second_record).unwrap();

    let first_history = first.list_revisions("greet").unwrap();
    let second_history = second.list_revisions("greet").unwrap();
    assert_eq!(first_history.len(), 1);
    assert_eq!(first_history[0].record.description, "says hello");
    assert_eq!(second_history.len(), 1);
    assert_eq!(second_history[0].record.description, "catalog b original");
}

#[test]
fn undo_restores_the_previous_version_and_is_itself_undoable() {
    let root = tempfile::tempdir().unwrap();
    let store = store_in(root.path());
    let mut record = parse_workflow(&valid_document("greet"), "greet").unwrap();
    store.save(&record).unwrap();
    record.description = "rewritten by the copilot".into();
    store.save(&record).unwrap();

    let (revision, restored) = undo_last(&store, "greet").unwrap().expect("history");

    assert_eq!(restored.description, "says hello");
    assert_eq!(
        store.get("greet").unwrap().unwrap().description,
        "says hello"
    );
    // The rollback went through `save`, so the version it replaced was
    // snapshotted too: pressing undo twice returns to where you started.
    let history = store.list_revisions("greet").unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].record.description, "rewritten by the copilot");
    assert_ne!(history[0].id, revision.id);
}

#[test]
fn rolling_back_to_a_named_revision_restores_exactly_that_one() {
    let root = tempfile::tempdir().unwrap();
    let store = store_in(root.path());
    let mut record = parse_workflow(&valid_document("greet"), "greet").unwrap();
    for description in ["first", "second", "third"] {
        record.description = description.into();
        store.save(&record).unwrap();
    }

    // Three saves, but the first replaced nothing — so history holds the two
    // versions that were superseded, newest first, and the last entry is the
    // oldest of those.
    let history = store.list_revisions("greet").unwrap();
    assert_eq!(history.len(), 2);
    let oldest = history.last().expect("history");
    let restored = rollback(&store, "greet", &oldest.id).unwrap();

    assert_eq!(restored.description, "first");
}

#[test]
fn rolling_back_to_a_revision_that_does_not_exist_is_an_error() {
    let root = tempfile::tempdir().unwrap();
    let store = store_in(root.path());
    store
        .save(&parse_workflow(&valid_document("greet"), "greet").unwrap())
        .unwrap();

    let err = rollback(&store, "greet", "no-such-revision").expect_err("must refuse");

    assert!(matches!(err, WorkflowError::Malformed(_)), "got {err:?}");
}

#[test]
fn deleting_a_workflow_leaves_it_recoverable() {
    let root = tempfile::tempdir().unwrap();
    let store = store_in(root.path());
    store
        .save(&parse_workflow(&valid_document("greet"), "greet").unwrap())
        .unwrap();

    store.delete("greet").unwrap();

    // A delete has nothing left to diff against, which is exactly why it is
    // snapshotted: without this it is the one edit that cannot be taken back.
    assert!(store.get("greet").unwrap().is_none());
    let history = store.list_revisions("greet").unwrap();
    assert_eq!(history.len(), 1);
    rollback(&store, "greet", &history[0].id).unwrap();
    assert!(store.get("greet").unwrap().is_some());
}

#[test]
fn history_does_not_show_up_in_the_workflow_listing() {
    let root = tempfile::tempdir().unwrap();
    let store = store_in(root.path());
    let mut record = parse_workflow(&valid_document("greet"), "greet").unwrap();
    store.save(&record).unwrap();
    record.description = "again".into();
    store.save(&record).unwrap();

    // Snapshots sit outside the definition directory, so a load must never
    // mistake a past version for a current workflow.
    assert_eq!(store.list().unwrap().len(), 1);
    assert!(store.load().errors.is_empty());
}

#[test]
fn shadowing_a_project_default_with_a_home_workflow_snapshots_what_it_shadowed() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let project = root.path().join("project");
    write(&project.join("greet.json"), &valid_document("greet"));
    let store = FileWorkflowStore::new(vec![project, home], root.path().join("runs"));

    // The first home-level save writes a file that did not exist, so nothing
    // in the write directory is overwritten — but the operator *does* see the
    // graph change, because the home copy now shadows the project default.
    let mut record = require(&store, "greet").unwrap();
    record.description = "edited in this project".into();
    store.save(&record).unwrap();

    let history = store.list_revisions("greet").unwrap();
    assert_eq!(history.len(), 1, "the shadowed version must be recoverable");
    assert_eq!(history[0].record.description, "says hello");
    rollback(&store, "greet", &history[0].id).unwrap();
    assert_eq!(
        store.get("greet").unwrap().unwrap().description,
        "says hello"
    );
}
