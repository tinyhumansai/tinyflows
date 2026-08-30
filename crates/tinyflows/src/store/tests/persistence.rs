//! Save/delete round-tripping: the graph and host fields survive a save, the
//! `defaults` block round-trips (or is omitted entirely when empty), an
//! uncompilable graph is refused, and deletes remove the file actually read.

use serde_json::json;

use super::*;

#[test]
fn saving_then_loading_round_trips_the_host_fields_and_the_graph() {
    let root = tempfile::tempdir().unwrap();
    let store = store_in(root.path());
    let mut record = parse_workflow(&valid_document("round"), "round").unwrap();
    record.description = "edited".into();
    record.enabled = false;

    store.save(&record).expect("saves");
    let loaded = require(&store, "round").expect("found");

    assert_eq!(loaded.description, "edited");
    assert!(!loaded.enabled, "enabled must survive the round trip");
    assert_eq!(loaded.graph, record.graph);
    assert_eq!(loaded.trigger_kind().as_deref(), Some("manual"));
}

#[test]
fn authored_directory_contains_only_workflow_sources_after_edits() {
    let root = tempfile::tempdir().unwrap();
    let store = store_in(root.path());
    let mut record = parse_workflow(&valid_document("clean"), "clean").unwrap();

    store.save(&record).expect("initial save");
    record.description = "second version".into();
    store.save(&record).expect("edit");

    let source_entries: Vec<_> = std::fs::read_dir(root.path().join("workflows"))
        .expect("source directory")
        .map(|entry| entry.expect("entry").file_name())
        .collect();
    assert_eq!(source_entries, vec![std::ffi::OsString::from("clean.json")]);
    let definition_state = super::super::file::definition_state_dir(
        &root.path().join("state/workflows"),
        &[root.path().join("workflows")],
    );
    assert!(definition_state.join("revisions/clean").is_dir());
    assert!(definition_state.join("locks/.clean.lock").is_file());
}

#[test]
fn explicit_state_root_owns_definition_history_and_locks() {
    let root = tempfile::tempdir().unwrap();
    let catalog_parent = tempfile::tempdir().unwrap();
    let definitions = catalog_parent.path().join("authored");
    let state = root.path().join("host-state");
    let store = FileWorkflowStore::with_state(vec![definitions.clone()], &state);
    let mut record = parse_workflow(&valid_document("placed"), "placed").unwrap();
    store.save(&record).unwrap();
    record.description = "edited".into();
    store.save(&record).unwrap();

    let definition_state =
        super::super::file::definition_state_dir(&state, std::slice::from_ref(&definitions));
    assert!(definition_state.join("revisions/placed").is_dir());
    assert!(definition_state.join("locks/.placed.lock").is_file());
    assert!(!catalog_parent.path().join("state").exists());
}

#[test]
fn saving_round_trips_the_defaults_block() {
    let root = tempfile::tempdir().unwrap();
    let store = store_in(root.path());
    let mut record = parse_workflow(&valid_document("pinned"), "pinned").unwrap();
    record.defaults.harness = Some("codex".into());
    record.defaults.model = Some("gpt-5-codex".into());

    store.save(&record).expect("saves");
    let loaded = require(&store, "pinned").expect("found");

    assert_eq!(loaded.defaults, record.defaults);
}

#[test]
fn a_workflow_stating_no_preference_writes_no_defaults_block() {
    // A document an operator opens should not grow a block of nulls to say
    // nothing.
    let root = tempfile::tempdir().unwrap();
    let store = store_in(root.path());
    let record = parse_workflow(&valid_document("plain"), "plain").unwrap();

    store.save(&record).expect("saves");
    let path = require(&store, "plain")
        .unwrap()
        .source_path
        .expect("on disk");
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();

    assert!(written.get("defaults").is_none(), "{written}");
}

#[test]
fn saving_refuses_a_graph_the_engine_would_not_compile() {
    let root = tempfile::tempdir().unwrap();
    let store = store_in(root.path());
    let record = WorkflowRecord {
        id: "no-trigger".into(),
        name: "no trigger".into(),
        description: String::new(),
        enabled: true,
        defaults: Default::default(),
        graph: serde_json::from_value(json!({ "nodes": [], "edges": [] })).unwrap(),
        source_path: None,
    };

    let err = store.save(&record).expect_err("must not persist");

    assert!(matches!(err, WorkflowError::Invalid { .. }), "got {err:?}");
    assert!(
        store.list().unwrap().is_empty(),
        "an invalid graph must not reach the catalog"
    );
}

#[test]
fn deleting_removes_the_file_the_workflow_was_actually_read_from() {
    let root = tempfile::tempdir().unwrap();
    let store = store_in(root.path());
    let record = parse_workflow(&valid_document("gone"), "gone").unwrap();
    store.save(&record).unwrap();

    store.delete("gone").expect("deletes");

    assert!(store.get("gone").unwrap().is_none());
    let err = store
        .delete("gone")
        .expect_err("deleting twice is an error");
    assert!(matches!(err, WorkflowError::NotFound(_)), "got {err:?}");
}

#[test]
fn deleting_a_repository_default_never_modifies_the_checkout() {
    let root = tempfile::tempdir().unwrap();
    let repository_dir = root.path().join("repo/.flows/workflows");
    let home_dir = root.path().join("home/workflows");
    let repository_file = repository_dir.join("shared.json");
    write(&repository_file, &valid_document("shared"));
    let store = FileWorkflowStore::new(
        vec![repository_dir, home_dir],
        root.path().join("state/runs"),
    );

    let err = store
        .delete("shared")
        .expect_err("repository defaults are read-only");

    assert!(
        matches!(err, WorkflowError::ReadOnlyDefinition { .. }),
        "got {err:?}"
    );
    assert!(
        repository_file.exists(),
        "the checkout must remain untouched"
    );
    assert!(store.get("shared").unwrap().is_some());
}

#[test]
fn deleting_a_home_definition_uses_its_actual_filename() {
    let root = tempfile::tempdir().unwrap();
    let home_dir = root.path().join("home/workflows");
    let alias_file = home_dir.join("alias.json");
    write(&alias_file, &valid_document("shared"));
    let store = FileWorkflowStore::new(vec![home_dir], root.path().join("state/runs"));

    store
        .delete("shared")
        .expect("home definitions are writable");

    assert!(!alias_file.exists());
    assert!(store.get("shared").unwrap().is_none());
}

#[test]
fn an_id_containing_a_dot_does_not_collide_on_its_temporary_file() {
    // The temp name is appended, not substituted for the extension, so
    // `a.b.json` and `a.json` cannot fight over one scratch path.
    let root = tempfile::tempdir().unwrap();
    let store = store_in(root.path());
    store
        .save(&parse_workflow(&valid_document("a.b"), "a.b").unwrap())
        .unwrap();
    store
        .save(&parse_workflow(&valid_document("a"), "a").unwrap())
        .unwrap();

    // Load order is the sorted filename order, so `a.b.json` precedes `a.json`.
    let ids: Vec<String> = store.list().unwrap().into_iter().map(|s| s.id).collect();
    assert_eq!(ids, vec!["a.b", "a"], "both should survive");
}
