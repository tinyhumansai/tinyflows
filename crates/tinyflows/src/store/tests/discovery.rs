//! Layered directory loading: a home definition overrides a project default in
//! place, one malformed document costs only itself, and a missing directory is
//! not an error.

use super::*;

#[test]
fn a_home_workflow_overrides_a_project_default_of_the_same_id_in_place() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let project = root.path().join("project");

    write(&project.join("first.json"), &valid_document("first"));
    write(&project.join("shared.json"), &valid_document("shared"));
    let overridden = valid_document("shared").replace("\"Greet\"", "\"Personal greet\"");
    write(&home.join("shared.json"), &overridden);

    let store = FileWorkflowStore::new(vec![project, home], root.path().join("runs"));
    let report = store.load();

    assert!(report.errors.is_empty(), "unexpected: {:?}", report.errors);
    let ids: Vec<&str> = report.workflows.iter().map(|w| w.id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["first", "shared"],
        "an override should keep the position of what it overrides"
    );
    assert_eq!(report.workflows[1].name, "Personal greet");
}

#[test]
fn one_malformed_document_costs_only_itself() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("workflows");
    write(&dir.join("good.json"), &valid_document("good"));
    write(&dir.join("broken.json"), "{ not json");

    let store = store_in(root.path());
    let report = store.load();

    assert_eq!(
        report.workflows.len(),
        1,
        "the good document should survive"
    );
    assert_eq!(report.errors.len(), 1);
    assert!(
        report.errors[0].contains("broken.json"),
        "the error should name the file: {:?}",
        report.errors
    );
}

#[test]
fn a_missing_directory_is_not_an_error() {
    let root = tempfile::tempdir().unwrap();
    let report = store_in(root.path()).load();

    assert!(report.workflows.is_empty());
    assert!(report.errors.is_empty(), "unexpected: {:?}", report.errors);
    assert!(report.dirs.is_empty(), "nothing was read");
}
