//! Unit tests for workflow directory layering: which directories hold
//! workflows, in what precedence, and which one a save lands in.

use std::path::{Path, PathBuf};

use super::super::{FileWorkflowStore, parse_workflow};
use super::workflow_dirs;
use crate::store::WorkflowStore;

/// The smallest document that validates: one trigger, one transform, one edge.
fn valid_document(id: &str) -> String {
    serde_json::json!({
        "id": id,
        "name": "Greet",
        "description": "says hello",
        "nodes": [
            { "id": "t", "kind": "trigger", "name": "start",
              "config": { "trigger_kind": "manual" } },
            { "id": "greet", "kind": "transform", "name": "greet",
              "config": { "set": { "greeting": "=.item.name" } } }
        ],
        "edges": [
            { "from_node": "t", "from_port": "main", "to_node": "greet", "to_port": "main" }
        ]
    })
    .to_string()
}

#[test]
fn the_directories_are_project_then_home_with_home_as_the_write_layer() {
    let dirs = workflow_dirs(Path::new("/somewhere/home"), Path::new("/repo"), ".myapp");

    assert_eq!(
        dirs,
        vec![
            PathBuf::from("/repo/.myapp/workflows"),
            PathBuf::from("/somewhere/home/workflows"),
        ]
    );
}

#[test]
fn a_discovered_store_saves_new_definitions_under_the_home() {
    // The precedence rule made observable: a project directory is a place to
    // *read* repository-provided defaults from, never a place an edit lands.
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let project = root.path().join("project");
    let store = FileWorkflowStore::discover(&home, &project, ".myapp");
    let record = parse_workflow(&valid_document("home-save"), "home-save").unwrap();

    store.save(&record).unwrap();

    assert!(home.join("workflows/home-save.json").is_file());
    assert!(!project.join(".myapp/workflows/home-save.json").exists());
}

#[test]
fn a_home_inside_the_project_directory_collapses_both_layers() {
    // The constraint `workflow_dirs` documents but cannot enforce: a host whose
    // home resolves inside `<cwd>/<project_dir>` reads one directory twice and
    // makes every workflow shadow itself.
    let dirs = workflow_dirs(Path::new("/repo/.myapp"), Path::new("/repo"), ".myapp");

    assert_eq!(dirs.len(), 2);
    assert_eq!(
        dirs[0], dirs[1],
        "a home inside the project directory collapses both layers onto one path"
    );
}

#[test]
fn a_home_outside_the_project_keeps_the_two_layers_distinct() {
    let dirs = workflow_dirs(Path::new("/somewhere/home"), Path::new("."), ".myapp");

    assert_eq!(dirs.len(), 2);
    assert_ne!(dirs[0], dirs[1]);
}
