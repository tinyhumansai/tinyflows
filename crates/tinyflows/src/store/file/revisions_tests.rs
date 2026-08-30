//! Tests for snapshot capture, ordering, pruning, and scoping.

use serde_json::json;

use super::*;
use crate::store::types::WorkflowRecord;

/// A record whose graph validates, named so successive versions are tellable
/// apart by their description.
fn record(id: &str, description: &str) -> WorkflowRecord {
    WorkflowRecord {
        id: id.to_string(),
        name: "Greet".into(),
        description: description.to_string(),
        enabled: true,
        defaults: Default::default(),
        graph: serde_json::from_value(json!({
            "name": "Greet",
            "nodes": [
                { "id": "t", "kind": "trigger", "name": "start",
                  "config": { "trigger_kind": "manual" } },
            ],
            "edges": [],
        }))
        .expect("graph parses"),
        source_path: None,
    }
}

#[test]
fn a_captured_snapshot_can_be_listed_and_read_back() {
    let root = tempfile::tempdir().expect("tempdir");

    capture(root.path(), &record("greet", "first")).expect("capture");

    let listed = list(root.path(), "greet").expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].record.description, "first");
    assert!(listed[0].superseded_at > 0);

    let one = read(root.path(), "greet", &listed[0].id)
        .expect("read")
        .expect("present");
    assert_eq!(one.record.description, "first");
}

#[test]
fn snapshots_are_listed_newest_first() {
    let root = tempfile::tempdir().expect("tempdir");

    for description in ["first", "second", "third"] {
        capture(root.path(), &record("greet", description)).expect("capture");
    }

    let listed = list(root.path(), "greet").expect("list");
    let descriptions: Vec<&str> = listed
        .iter()
        .map(|r| r.record.description.as_str())
        .collect();
    assert_eq!(descriptions, vec!["third", "second", "first"]);
}

#[test]
fn two_snapshots_taken_in_the_same_millisecond_are_both_kept() {
    let root = tempfile::tempdir().expect("tempdir");

    // No sleep between them on purpose: the stamp alone is not unique enough to
    // name a file, so a second save inside one millisecond used to overwrite the
    // first rather than adding to the history.
    capture(root.path(), &record("greet", "a")).expect("capture");
    capture(root.path(), &record("greet", "b")).expect("capture");

    assert_eq!(list(root.path(), "greet").expect("list").len(), 2);
}

#[test]
fn history_is_capped_and_the_oldest_go_first() {
    let root = tempfile::tempdir().expect("tempdir");

    for n in 0..MAX_REVISIONS + 5 {
        let captured = capture(root.path(), &record("greet", &format!("v{n}"))).expect("capture");
        commit_capture(&captured).expect("commit capture");
    }

    let listed = list(root.path(), "greet").expect("list");
    assert_eq!(listed.len(), MAX_REVISIONS);
    assert_eq!(
        listed[0].record.description,
        format!("v{}", MAX_REVISIONS + 4)
    );
    // The five oldest were dropped, so the tail starts at v5 rather than v0.
    assert_eq!(listed[MAX_REVISIONS - 1].record.description, "v5");
}

#[test]
fn one_workflow_cannot_read_another_workflows_history() {
    let root = tempfile::tempdir().expect("tempdir");
    capture(root.path(), &record("greet", "secret")).expect("capture");
    let listed = list(root.path(), "greet").expect("list");

    // The id names a real file — just not one that belongs to this workflow.
    // Letting it through would be a way to write a graph the operator never had.
    let cross = read(root.path(), "other", &listed[0].id).expect("read");

    assert!(cross.is_none());
}

#[test]
fn a_workflow_with_no_history_lists_nothing_rather_than_failing() {
    let root = tempfile::tempdir().expect("tempdir");

    assert!(list(root.path(), "never-edited").expect("list").is_empty());
    assert!(
        read(root.path(), "never-edited", "whatever")
            .expect("read")
            .is_none()
    );
}

#[test]
fn a_snapshot_forgets_where_the_record_was_read_from() {
    let root = tempfile::tempdir().expect("tempdir");
    let mut original = record("greet", "first");
    original.source_path = Some("/somewhere/greet.json".into());

    capture(root.path(), &original).expect("capture");

    // Carrying the path would make a rollback claim to have come from a file
    // that by then holds something else.
    let listed = list(root.path(), "greet").expect("list");
    assert_eq!(listed[0].record.source_path, None);
}

#[test]
fn current_and_legacy_histories_are_merged_newest_first() {
    let root = tempfile::tempdir().expect("tempdir");
    let current = root.path().join("current");
    let legacy = root.path().join("legacy");
    capture(&legacy, &record("greet", "legacy")).expect("legacy capture");
    capture(&current, &record("greet", "current")).expect("current capture");

    let listed = list_merged(&current, &legacy, "greet").expect("merged history");

    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].record.description, "current");
    assert_eq!(listed[1].record.description, "legacy");
}

#[test]
fn an_id_that_would_escape_the_history_directory_is_refused() {
    let root = tempfile::tempdir().expect("tempdir");

    assert!(capture(root.path(), &record("../escape", "x")).is_err());
    assert!(list(root.path(), "../escape").is_err());
    assert!(read(root.path(), "greet", "../../escape").is_err());
}
