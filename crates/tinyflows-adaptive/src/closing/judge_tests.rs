use super::*;

/// A provider that answers the judge with a fixed blocker.
struct Says(&'static str);

#[async_trait::async_trait]
impl tinyflows::caps::LlmProvider for Says {
    async fn complete(
        &self,
        _request: serde_json::Value,
        _conn: Option<&str>,
    ) -> tinyflows::error::Result<serde_json::Value> {
        Ok(serde_json::json!({
            "satisfied": false,
            "blocker": self.0,
            "gap": "nothing was fetched",
        }))
    }
}

async fn verdict_for(blocker: &'static str, failed: Option<String>) -> Verdict {
    let outcome = tinyflows::engine::RunOutcome {
        // Non-empty: the mechanical pre-judge must not settle this one,
        // because the point is what the MODEL's answer becomes.
        output: serde_json::json!({ "nodes": { "fetch": { "json": 1 } } }),
        pending_approvals: Vec::new(),
        cancelled: false,
    };
    let diagnosis = Diagnosis::default();
    let evidence = Evidence {
        outcome: &outcome,
        diagnosis: &diagnosis,
        changed: String::new(),
        failed,
    };
    let caps = tinyflows::caps::Capabilities {
        llm: std::sync::Arc::new(Says(blocker)),
        ..tinyflows::caps::mock::mock_capabilities()
    };
    judge(&Goal::new("do the thing"), &evidence, &caps, None)
        .await
        .expect("judged")
}

#[tokio::test]
async fn a_mechanically_broken_run_cannot_be_called_terminal() {
    // Field observation: a shell step exited nonzero, the judge answered
    // `missing_evidence`, and the episode ended with two of its three
    // attempts unused — when rewriting the script was the whole fix.
    // The prompt says mechanical failures are goal_not_met; a model that
    // misreads it must not get to end the episode anyway.
    let verdict = verdict_for("missing_evidence", Some("script exited 5".into())).await;
    assert_eq!(verdict.blocker, Blocker::GoalNotMet);
    assert!(verdict.blocker.continuable());
}

#[tokio::test]
async fn a_run_that_completed_keeps_the_judges_terminal_verdict() {
    // No mechanical failure: the judge is the authority on whether
    // another attempt could help, and this downgrade must not become a
    // blanket refusal to ever stand down.
    let verdict = verdict_for("missing_evidence", None).await;
    assert_eq!(verdict.blocker, Blocker::MissingEvidence);
}

#[tokio::test]
async fn a_broken_run_still_waiting_on_a_person_stays_terminal() {
    // NeedsInput and ExternalWait survive the downgrade: both mean
    // something OUTSIDE the loop must move, which a broken run does not
    // change.
    let verdict = verdict_for("needs_input", Some("script exited 5".into())).await;
    assert_eq!(verdict.blocker, Blocker::NeedsInput);
}

use serde_json::json;
use tinyflows::diagnostics::{HiddenError, NeverRan, NullBinding};

fn outcome(output: serde_json::Value) -> RunOutcome {
    RunOutcome {
        output,
        pending_approvals: Vec::new(),
        cancelled: false,
    }
}

fn evidence<'a>(o: &'a RunOutcome, d: &'a Diagnosis) -> Evidence<'a> {
    Evidence {
        outcome: o,
        diagnosis: d,
        changed: String::new(),
        failed: None,
    }
}

#[test]
fn a_parked_approval_needs_no_model() {
    let mut o = outcome(json!({}));
    o.pending_approvals = vec!["gate".into()];
    let d = Diagnosis::default();
    let verdict = without_a_model(&evidence(&o, &d)).expect("settled without a model");
    assert_eq!(verdict.blocker, Blocker::NeedsInput);
    assert!(
        verdict.advanced,
        "reaching the gate is progress, not a stall"
    );
}

#[test]
fn a_cancelled_run_did_not_fail_it_was_stopped() {
    let mut o = outcome(json!({ "nodes": { "a": {} } }));
    o.cancelled = true;
    let d = Diagnosis::default();
    let verdict = without_a_model(&evidence(&o, &d)).expect("settled");
    assert_eq!(verdict.blocker, Blocker::ExternalWait);
    assert!(
        !verdict.blocker.continuable(),
        "retrying now is not retrying later"
    );
}

#[test]
fn a_run_where_nothing_ran_and_nothing_changed_is_terminal() {
    let o = outcome(json!({}));
    let d = Diagnosis {
        never_ran: vec![NeverRan {
            node_id: "work".into(),
            routed_by: Some("gate".into()),
        }],
        ..Diagnosis::default()
    };
    let verdict = without_a_model(&evidence(&o, &d)).expect("settled");
    assert_eq!(verdict.blocker, Blocker::MissingEvidence);
    assert!(!verdict.blocker.continuable());
}

#[test]
fn a_run_that_produced_something_goes_to_the_model() {
    let o = outcome(json!({ "nodes": { "a": { "items": [1] } } }));
    let d = Diagnosis::default();
    assert!(
        without_a_model(&evidence(&o, &d)).is_none(),
        "a real outcome is a judgement, not a fact"
    );
}

#[test]
fn an_unverifiable_null_binding_is_not_reported_as_a_finding() {
    // The engine marks expressions it could not evaluate even in principle.
    // Reporting those buries the ones that are real.
    let o = outcome(json!({}));
    let d = Diagnosis {
        null_bindings: vec![NullBinding {
            node_id: "a".into(),
            location: "config.prompt".into(),
            expression: "=nodes.x.item".into(),
            unverifiable: true,
            reads_from: None,
            suggestion: "n/a".into(),
        }],
        ..Diagnosis::default()
    };
    assert!(evidence(&o, &d).findings().is_empty());
}

#[test]
fn a_swallowed_error_reaches_the_judge() {
    // The failure a naive reading misses entirely: the step is marked
    // failed and its diagnostics are empty.
    let o = outcome(json!({}));
    let d = Diagnosis {
        hidden_errors: vec![HiddenError {
            node_id: "fetch".into(),
            message: Some("404".into()),
        }],
        ..Diagnosis::default()
    };
    let findings = evidence(&o, &d).findings();
    assert_eq!(findings.len(), 1);
    assert!(findings[0].contains("swallowed"), "{findings:?}");
    assert!(findings[0].contains("404"));
}

#[test]
fn a_null_binding_names_the_node_it_should_have_read_from() {
    let o = outcome(json!({}));
    let d = Diagnosis {
        null_bindings: vec![NullBinding {
            node_id: "review".into(),
            location: "config.prompt".into(),
            expression: "=nodes.fetch.item.body".into(),
            unverifiable: false,
            reads_from: Some("fetch".into()),
            suggestion: "did you mean .item.json.body".into(),
        }],
        ..Diagnosis::default()
    };
    let findings = evidence(&o, &d).findings();
    assert!(findings[0].contains("reading from `fetch`"), "{findings:?}");
    assert!(
        findings[0].contains("item.json.body"),
        "the suggestion carries"
    );
}

#[test]
fn a_clean_run_says_so_rather_than_showing_an_empty_list() {
    let o = outcome(json!({ "nodes": {} }));
    let d = Diagnosis::default();
    assert!(evidence(&o, &d).render().contains("found nothing wrong"));
}
