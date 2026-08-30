//! Tests for authoring suggestion upsert/list/status updates.

use super::*;
use crate::flows::test_support::*;
use tempfile::TempDir;

fn sample_suggestion(id: &str, title: &str) -> FlowSuggestion {
    FlowSuggestion {
        id: id.to_string(),
        title: title.to_string(),
        one_liner: "does a useful thing".to_string(),
        rationale: "grounded in your data".to_string(),
        trigger_hint: Some("schedule".to_string()),
        steps_outline: vec!["step one".to_string(), "step two".to_string()],
        suggested_connections: vec!["composio:gmail:conn_1".to_string()],
        suggested_slugs: vec!["GMAIL_SEND_EMAIL".to_string()],
        build_prompt: "Build a workflow that…".to_string(),
        confidence: 0.7,
        status: SuggestionStatus::New,
        created_at: "2026-07-05T00:00:00Z".to_string(),
        source_run_id: Some("run-1".to_string()),
    }
}

#[test]
fn suggestions_upsert_list_roundtrip() {
    let tmp = TempDir::new().unwrap();
    let dir = test_dir(&tmp);

    let written = upsert_suggestions(
        &dir,
        &[
            sample_suggestion("s1", "Alpha"),
            sample_suggestion("s2", "Beta"),
        ],
    )
    .unwrap();
    assert_eq!(written, 2);

    let all = list_suggestions(&dir, Some(SuggestionStatus::New), 50).unwrap();
    assert_eq!(all.len(), 2);
    // Round-trips the JSON-encoded vec columns.
    let alpha = all.iter().find(|s| s.id == "s1").unwrap();
    assert_eq!(alpha.steps_outline.len(), 2);
    assert_eq!(alpha.suggested_connections, vec!["composio:gmail:conn_1"]);
    assert_eq!(alpha.suggested_slugs, vec!["GMAIL_SEND_EMAIL"]);
    assert_eq!(alpha.trigger_hint.as_deref(), Some("schedule"));
}

#[test]
fn upsert_suggestions_preserves_user_status_on_rerun() {
    let tmp = TempDir::new().unwrap();
    let dir = test_dir(&tmp);

    upsert_suggestions(&dir, &[sample_suggestion("s1", "Alpha")]).unwrap();
    // User dismisses it.
    assert!(set_suggestion_status(&dir, "s1", SuggestionStatus::Dismissed).unwrap());

    // A later discovery run re-proposes the identical idea (same id) with a
    // refreshed pitch — the dismissal must survive.
    let mut refreshed = sample_suggestion("s1", "Alpha (refined)");
    refreshed.status = SuggestionStatus::New; // agent always emits `New`
    upsert_suggestions(&dir, &[refreshed]).unwrap();

    let dismissed = list_suggestions(&dir, Some(SuggestionStatus::Dismissed), 50).unwrap();
    assert_eq!(dismissed.len(), 1);
    assert_eq!(dismissed[0].title, "Alpha (refined)"); // pitch fields refreshed
    // …but it is NOT back in the active `New` list.
    let active = list_suggestions(&dir, Some(SuggestionStatus::New), 50).unwrap();
    assert!(active.is_empty());
}

#[test]
fn set_suggestion_status_returns_false_for_unknown_id() {
    let tmp = TempDir::new().unwrap();
    let dir = test_dir(&tmp);
    assert!(!set_suggestion_status(&dir, "missing", SuggestionStatus::Built).unwrap());
}

#[test]
fn list_suggestions_without_status_returns_all() {
    let tmp = TempDir::new().unwrap();
    let dir = test_dir(&tmp);
    upsert_suggestions(&dir, &[sample_suggestion("s1", "Alpha")]).unwrap();
    set_suggestion_status(&dir, "s1", SuggestionStatus::Built).unwrap();
    // Filtered to `New` → empty; unfiltered → present.
    assert!(
        list_suggestions(&dir, Some(SuggestionStatus::New), 50)
            .unwrap()
            .is_empty()
    );
    assert_eq!(list_suggestions(&dir, None, 50).unwrap().len(), 1);
}

#[test]
fn upsert_suggestions_empty_is_noop() {
    let tmp = TempDir::new().unwrap();
    let dir = test_dir(&tmp);
    assert_eq!(upsert_suggestions(&dir, &[]).unwrap(), 0);
}

// ── Orphaned-running-run reconciliation (bug B42) ──────────────────────────
