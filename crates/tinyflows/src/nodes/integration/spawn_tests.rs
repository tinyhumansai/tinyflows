use super::*;
use serde_json::json;

#[test]
fn every_target_shape_parses() {
    assert!(matches!(
        spec_from_config(&json!({ "target": "tool", "slug": "a.b" }), "s"),
        Ok(TaskSpec::Tool { .. })
    ));
    assert!(matches!(
        spec_from_config(&json!({ "target": "http", "request": {} }), "s"),
        Ok(TaskSpec::Http { .. })
    ));
    assert!(matches!(
        spec_from_config(&json!({ "target": "workflow", "workflow": {} }), "s"),
        Ok(TaskSpec::Workflow { .. })
    ));
}

/// A misconfigured spawn fails at the node rather than starting something
/// unintended — the error names the node and what was expected.
#[test]
fn a_missing_or_unknown_target_is_refused() {
    for config in [
        json!({}),
        json!({ "target": "sideways" }),
        json!({ "target": "tool" }),     // no slug
        json!({ "target": "workflow" }), // no graph
    ] {
        assert!(
            spec_from_config(&config, "s").is_err(),
            "config {config} should be refused"
        );
    }
}

#[test]
fn an_inline_ticket_reports_its_result_without_a_runner() {
    let item = json!({ "inline": true, "result": { "ok": true } });
    assert_eq!(
        inline_result(&item),
        Some(TaskState::Done(json!({ "ok": true })))
    );
    assert_eq!(inline_result(&json!({ "ticket": "task-1" })), None);
}
