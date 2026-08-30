//! Tests for the programmable capability doubles.
//!
//! Exercised through the trait methods directly rather than through a run: what
//! is under test is rule matching, sequencing, and logging, and driving a whole
//! graph to reach them would be testing the engine instead.

use super::*;
use crate::caps::{ApprovalOutcome, ApprovalRequest, ApprovalSubject};
use serde_json::json;

fn mocks(build: impl FnOnce(MockCaps) -> MockCaps) -> Arc<MockCaps> {
    Arc::new(build(MockCaps::new()))
}

#[test]
fn a_bare_glob_matches_anything() {
    assert!(glob_matches("*", "anything at all"));
    assert!(glob_matches("*", ""));
}

#[test]
fn a_glob_without_a_star_is_an_exact_match() {
    assert!(glob_matches("slack.send", "slack.send"));
    assert!(!glob_matches("slack.send", "slack.sendFile"));
    assert!(!glob_matches("slack.send", "slack"));
}

#[test]
fn a_trailing_star_matches_a_prefix() {
    assert!(glob_matches("gh.issues.*", "gh.issues.create"));
    assert!(glob_matches("gh.issues.*", "gh.issues."));
    assert!(!glob_matches("gh.issues.*", "gh.pulls.create"));
}

#[test]
fn a_leading_star_matches_a_suffix() {
    assert!(glob_matches("*.create", "gh.issues.create"));
    assert!(!glob_matches("*.create", "gh.issues.delete"));
}

#[test]
fn an_inner_star_matches_around_a_literal() {
    assert!(glob_matches(
        "https://*/webhook",
        "https://example.com/webhook"
    ));
    assert!(!glob_matches(
        "https://*/webhook",
        "https://example.com/other"
    ));
}

#[tokio::test]
async fn a_programmed_tool_returns_its_value() {
    let mocks = mocks(|m| m.on_tool("slack.send", Respond::value(json!({ "ok": true }))));
    let caps = mocks.capabilities();

    let out = caps
        .tools
        .invoke("slack.send", json!({ "text": "hi" }), None)
        .await
        .expect("programmed call succeeds");

    assert_eq!(out, json!({ "ok": true }));
}

#[tokio::test]
async fn an_unprogrammed_call_falls_back_to_the_echo() {
    // A graph reaching a capability nobody programmed must not fail for that
    // reason: the failure would say nothing about the graph.
    let mocks = mocks(|m| m);
    let caps = mocks.capabilities();

    let out = caps
        .tools
        .invoke("unmocked.tool", json!({ "a": 1 }), None)
        .await
        .expect("an unprogrammed call still answers");

    assert_eq!(out["tool"], json!("unmocked.tool"));
    assert_eq!(out["args"], json!({ "a": 1 }));
}

#[tokio::test]
async fn a_sequence_answers_successive_calls_in_order() {
    let mocks = mocks(|m| {
        m.on_tool(
            "gh.issues",
            Respond::sequence([
                Respond::error("429 rate limited"),
                Respond::value(json!({ "number": 7 })),
            ]),
        )
    });
    let caps = mocks.capabilities();

    let first = caps.tools.invoke("gh.issues", json!({}), None).await;
    assert!(first.is_err(), "the first call should fail");
    assert!(first.unwrap_err().to_string().contains("429"));

    let second = caps
        .tools
        .invoke("gh.issues", json!({}), None)
        .await
        .expect("the retry succeeds");
    assert_eq!(second, json!({ "number": 7 }));
}

#[tokio::test]
async fn a_sequence_repeats_its_last_entry_rather_than_falling_through() {
    // Past the end the author's intent is "and thereafter", not "and then
    // something I never wrote".
    let mocks = mocks(|m| m.on_tool("t", Respond::sequence([Respond::value(json!("only"))])));
    let caps = mocks.capabilities();

    for _ in 0..3 {
        let out = caps.tools.invoke("t", json!({}), None).await.expect("call");
        assert_eq!(out, json!("only"));
    }
}

#[tokio::test]
async fn the_first_matching_rule_wins() {
    let mocks = mocks(|m| {
        m.on_tool("gh.issues.create", Respond::value(json!("specific")))
            .on_tool("gh.*", Respond::value(json!("general")))
    });
    let caps = mocks.capabilities();

    let specific = caps
        .tools
        .invoke("gh.issues.create", json!({}), None)
        .await
        .expect("call");
    assert_eq!(specific, json!("specific"));

    let general = caps
        .tools
        .invoke("gh.pulls.merge", json!({}), None)
        .await
        .expect("call");
    assert_eq!(general, json!("general"));
}

#[tokio::test]
async fn a_schema_rule_synthesizes_a_conforming_value() {
    let mocks = mocks(|m| {
        m.on_tool(
            "shaped",
            Respond::schema(json!({
                "type": "object",
                "properties": { "name": { "type": "string" }, "count": { "type": "integer" } }
            })),
        )
    });
    let caps = mocks.capabilities();

    let out = caps
        .tools
        .invoke("shaped", json!({}), None)
        .await
        .expect("call");
    assert_eq!(out, json!({ "name": "sample", "count": 0 }));
}

#[tokio::test]
async fn every_call_is_logged_in_one_sequence_across_capabilities() {
    // The single sequence is the point: per-capability counters cannot say
    // whether the HTTP call happened before or after the tool call.
    let mocks = mocks(|m| m);
    let caps = mocks.capabilities();

    caps.tools
        .invoke("first.tool", json!({ "a": 1 }), None)
        .await
        .expect("call");
    caps.http
        .request(json!({ "url": "https://example.com/x" }), None)
        .await
        .expect("call");

    let calls = mocks.log().calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].seq, 0);
    assert_eq!(calls[0].capability, capability::TOOLS);
    assert_eq!(calls[0].target, "first.tool");
    assert_eq!(calls[0].args, json!({ "a": 1 }));
    assert_eq!(calls[1].seq, 1);
    assert_eq!(calls[1].capability, capability::HTTP);
    assert_eq!(calls[1].target, "https://example.com/x");
}

#[tokio::test]
async fn a_failed_call_is_logged_with_its_message() {
    let mocks = mocks(|m| m.on_tool("boom", Respond::error("exploded")));
    let caps = mocks.capabilities();

    let _ = caps.tools.invoke("boom", json!({}), None).await;

    let calls = mocks.log().calls();
    assert_eq!(calls.len(), 1, "a failure is still a call that happened");
    assert_eq!(
        calls[0].outcome,
        CallOutcome::Err("capability error: exploded".to_string())
    );
}

#[tokio::test]
async fn counting_and_filtering_the_log() {
    let mocks = mocks(|m| m);
    let caps = mocks.capabilities();

    caps.tools
        .invoke("gh.a", json!({}), None)
        .await
        .expect("call");
    caps.tools
        .invoke("gh.b", json!({}), None)
        .await
        .expect("call");
    caps.tools
        .invoke("slack.send", json!({}), None)
        .await
        .expect("call");

    let log = mocks.log();
    assert_eq!(log.count(capability::TOOLS, None), 3);
    assert_eq!(log.count(capability::TOOLS, Some("gh.*")), 2);
    assert_eq!(log.count(capability::TOOLS, Some("slack.send")), 1);
    assert_eq!(log.count(capability::HTTP, None), 0);
}

#[tokio::test]
async fn a_node_scoped_bundle_attributes_its_calls() {
    let mocks = mocks(|m| m);
    let caps = mocks.capabilities_for_node("send_email");

    caps.tools
        .invoke("slack.send", json!({}), None)
        .await
        .expect("call");

    let calls = mocks.log().calls();
    assert_eq!(calls[0].node_id.as_deref(), Some("send_email"));
}

#[tokio::test]
async fn a_rule_can_be_restricted_to_one_node() {
    let mocks = mocks(|m| {
        m.on_tool("svc.do", Respond::value(json!("stubbed")))
            .only_from("node_a")
    });

    let scoped = mocks.capabilities_for_node("node_a");
    let out = scoped
        .tools
        .invoke("svc.do", json!({}), None)
        .await
        .expect("call");
    assert_eq!(out, json!("stubbed"));

    // The same slug from a different node falls through to the default, so
    // stubbing one node leaves the rest of the graph alone.
    let other = mocks.capabilities_for_node("node_b");
    let out = other
        .tools
        .invoke("svc.do", json!({}), None)
        .await
        .expect("call");
    assert_eq!(out["tool"], json!("svc.do"));
}

#[tokio::test]
async fn the_state_store_really_stores() {
    // The one capability whose whole job is to remember: a rule that overrode a
    // load would make a stateful graph unreadable, so loads read the map.
    let mocks = mocks(|m| m);
    let caps = mocks.capabilities();

    assert_eq!(caps.state.load("k").await.expect("load"), None);
    caps.state.store("k", json!(42)).await.expect("store");
    assert_eq!(caps.state.load("k").await.expect("load"), Some(json!(42)));

    assert_eq!(mocks.log().count(capability::STATE, None), 3);
}

/// `capabilities_for_node` builds a fresh `Capabilities` bundle — and so a
/// fresh `Double` — on every node activation, so it can attribute calls to
/// the right node. State must not live on that per-activation `Double`, or
/// nothing written by one activation would be visible to the next: not a
/// later activation of the SAME node (a loop reading what an earlier
/// iteration stored), and not a different node reading what an upstream one
/// wrote.
#[tokio::test]
async fn state_persists_across_node_scoped_bundles() {
    let mocks = mocks(|m| m);

    // Node "writer"'s first activation stores a value...
    mocks
        .capabilities_for_node("writer")
        .state
        .store("k", json!("from writer"))
        .await
        .expect("store");

    // ...a later activation of the SAME node must still see it...
    assert_eq!(
        mocks
            .capabilities_for_node("writer")
            .state
            .load("k")
            .await
            .expect("load"),
        Some(json!("from writer")),
        "a node's own later activation must see what an earlier one stored"
    );

    // ...and so must a DIFFERENT node's bundle, entirely.
    assert_eq!(
        mocks
            .capabilities_for_node("reader")
            .state
            .load("k")
            .await
            .expect("load"),
        Some(json!("from writer")),
        "state is one store shared across the whole run, not one per node"
    );
}

#[tokio::test]
async fn a_delayed_response_still_answers() {
    let mocks = mocks(|m| {
        m.on_tool(
            "slow",
            Respond::after(Duration::from_millis(1), Respond::value(json!("late"))),
        )
    });
    let caps = mocks.capabilities();

    let out = caps
        .tools
        .invoke("slow", json!({}), None)
        .await
        .expect("call");
    assert_eq!(out, json!("late"));
}

#[tokio::test]
async fn an_unregistered_sub_workflow_is_refused_by_name() {
    let mocks = mocks(|m| m);
    let caps = mocks.capabilities();

    let err = caps
        .resolver
        .resolve("nope")
        .await
        .expect_err("an unregistered id should not silently resolve");
    assert!(err.to_string().contains("nope"), "got {err}");
}

fn approval_request(request_id: &str) -> ApprovalRequest {
    ApprovalRequest {
        request_id: request_id.to_string(),
        node_id: "review".to_string(),
        run_id: Some("run-1".to_string()),
        title: Some("Ship it?".to_string()),
        prompt: None,
        subject: ApprovalSubject {
            kind: "url".to_string(),
            value: json!("https://example.com/preview"),
        },
        assignees: vec!["reviewer@example.com".to_string()],
        metadata: json!({}),
    }
}

#[tokio::test]
async fn an_unprogrammed_review_approves_and_is_logged() {
    // Same bargain as every other default here: a graph containing a review
    // runs end to end without a test standing a reviewer up, and the call still
    // shows in the log so a test can assert the review was *asked for*.
    let mocks = mocks(|m| m);
    let caps = mocks.capabilities();
    let approvals = caps.approvals.clone().expect("the doubles wire approvals");

    let outcome = approvals
        .decide(&approval_request("run-1:review"))
        .await
        .expect("an unprogrammed review still answers");

    match outcome {
        ApprovalOutcome::Decided(decision) => {
            assert!(decision.approved);
            assert_eq!(decision.decided_by.as_deref(), Some("testkit"));
        }
        ApprovalOutcome::Pending => panic!("the default should decide, not park the run"),
    }

    let calls = mocks.log().calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].capability, capability::APPROVALS);
    assert_eq!(calls[0].target, "run-1:review");
    assert_eq!(calls[0].args["subject"]["kind"], json!("url"));
}

#[tokio::test]
async fn a_programmed_review_can_reject_or_stay_pending() {
    // Rules match on `request_id`, which is what lets one test settle one
    // review and leave another waiting — the two halves of the wait paths a
    // review node has to cope with.
    let mocks = mocks(|m| {
        m.on_approval(
            "run-1:reject*",
            Respond::value(json!({ "approved": false, "comment": "not yet" })),
        )
        .on_approval(
            "run-1:slow*",
            Respond::value(json!({ "status": "pending" })),
        )
    });
    let caps = mocks.capabilities();
    let approvals = caps.approvals.clone().expect("the doubles wire approvals");

    let rejected = approvals
        .decide(&approval_request("run-1:rejected-review"))
        .await
        .expect("call");
    match rejected {
        ApprovalOutcome::Decided(decision) => {
            assert!(!decision.approved);
            assert_eq!(decision.comment.as_deref(), Some("not yet"));
        }
        ApprovalOutcome::Pending => panic!("a programmed verdict should decide"),
    }

    let pending = approvals
        .decide(&approval_request("run-1:slow-review"))
        .await
        .expect("call");
    assert_eq!(
        pending,
        ApprovalOutcome::Pending,
        "`status: pending` is how a test exercises a poll or a suspend"
    );

    approvals
        .cancel("run-1:slow-review", "run ended")
        .await
        .expect("cancelling a review is answered too");
    assert_eq!(mocks.log().count(capability::APPROVALS, None), 3);
}

/// The bare string `"pending"` is documented shorthand for "nobody has got to
/// this review yet" — the same flexibility `on_shell` accepts a bare stdout
/// string for. A rule answering with it must not be read as an (approving)
/// verdict object with no recognized fields.
#[tokio::test]
async fn on_approval_accepts_a_bare_pending_string() {
    let mocks = mocks(|m| m.on_approval("run-1:bare*", Respond::value(json!("pending"))));
    let caps = mocks.capabilities();
    let approvals = caps.approvals.clone().expect("the doubles wire approvals");

    let outcome = approvals
        .decide(&approval_request("run-1:bare-review"))
        .await
        .expect("call");
    assert_eq!(
        outcome,
        ApprovalOutcome::Pending,
        "a bare \"pending\" string must not be read as an approving verdict"
    );
}
