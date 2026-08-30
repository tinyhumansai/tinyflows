/// Without an explicit `request_id` and without a run-scoped identity the node
/// must refuse to guess: falling back to the bare node id would let a later
/// run of the same graph reuse an earlier run's decision through the
/// provider's create-or-fetch contract, and route an unreviewed subject
/// straight through `approved`.
#[tokio::test]
async fn a_missing_request_id_and_run_id_is_a_configuration_error() {
    let graph = wf_raw(json!({ "title": "Publish this?" }));
    let compiled = compile(&graph).expect("compile");
    let err = run(&compiled, Value::Null, &mock_capabilities())
        .await
        .expect_err("no request_id and no run id must be refused");
    let message = err.to_string();
    assert!(message.contains("request_id"), "got {message}");
}

/// `validate::validate_all` only sees the config as authored — a
/// one-element `assignees` array of `=`-bindings passes it. If every binding
/// resolves to something that is not a string (here: a field the trigger
/// input never set, so the binding reads `null`), the array a real review
/// would be routed to is empty at execution time, and that is refused too.
#[tokio::test]
async fn assignees_resolving_to_no_strings_at_runtime_is_a_configuration_error() {
    let graph = wf(json!({ "assignees": ["=item.missing_reviewer"] }));
    let compiled = compile(&graph).expect("compile");
    let err = run(&compiled, json!({}), &mock_capabilities())
        .await
        .expect_err("assignees resolving to zero strings must be refused");
    let message = err.to_string();
    assert!(message.contains("assignees"), "got {message}");
}

/// A `run_id` in the **trigger payload** must NOT become the review's identity.
///
/// The trigger is caller-supplied — for a webhook or any user-facing trigger it
/// is attacker-influenced — and `request_id` is the provider's create-or-fetch
/// key. Honouring it would let an attacker name a review that collides with an
/// earlier run's and inherit its cached decision, approving an unreviewed
/// subject with no human involved. So the node refuses rather than deriving an
/// id it cannot trust.
#[tokio::test]
async fn a_run_id_in_the_trigger_payload_is_not_trusted_as_the_review_identity() {
    let graph = wf_raw(json!({ "title": "Publish this?" }));
    let compiled = compile(&graph).expect("compile");
    let err = run(
        &compiled,
        json!({ "run_id": "run-42" }),
        &mock_capabilities(),
    )
    .await
    .expect_err("a trigger-supplied run id must not be accepted as a review identity");

    let message = err.to_string();
    assert!(
        message.contains("run-scoped identity"),
        "the error must say what is missing, got {message}"
    );
    assert!(
        !message.contains("run-42"),
        "the untrusted value must not be echoed as though it were usable, got {message}"
    );
}

/// The supported way to identify a review when the host seeds no run id:
/// an explicit `config.request_id`, which the author controls.
#[tokio::test]
async fn an_explicit_request_id_identifies_the_review() {
    let graph = wf_raw(json!({ "title": "Publish this?", "request_id": "review-of-post-42" }));
    let compiled = compile(&graph).expect("compile");
    let out = run(&compiled, json!({ "url": "https://example.com" }), &mock_capabilities())
        .await
        .expect("an explicit request_id makes the review identifiable");

    assert_eq!(
        out.output["nodes"]["review"]["items"][0]["json"]["request_id"],
        "review-of-post-42"
    );
}

/// `on_reject: "drop"` emits nothing — a regression that accidentally emitted
/// an item here would go unnoticed by every other rejection test, which all
/// use `route`.
#[tokio::test]
async fn on_reject_drop_emits_nothing_but_still_records_the_decision() {
    let graph = wf(json!({ "on_reject": "drop" }));
    let compiled = compile(&graph).expect("compile");
    let out = run(
        &compiled,
        Value::Null,
        &mock_capabilities_with_approvals(MockApprovals::rejecting()),
    )
    .await
    .expect("run");

    let slot = &out.output["nodes"]["review"];
    assert_eq!(
        slot["items"],
        json!([]),
        "on_reject: drop must not emit an item"
    );
    assert_eq!(
        slot["decision"]["approved"],
        json!(false),
        "the verdict stays addressable as =nodes.review.decision.approved even when dropped"
    );
}

/// `on_timeout: "reject"` hands the timed-out review to the `on_reject`
/// policy. Covers all three `on_reject` sub-paths so a regression in any one
/// of them is caught here rather than by a host.
#[tokio::test]
async fn on_timeout_reject_follows_the_on_reject_policy() {
    // route: timed_out item lands on `rejected`, with the fields a settled
    // review always carries (not just `approved`/`timed_out`).
    let graph = wf(json!({
        "wait_mode": "poll",
        "poll_interval_ms": 1,
        "max_polls": 1,
        "on_timeout": "reject",
    }));
    let compiled = compile(&graph).expect("compile");
    let out = run(
        &compiled,
        Value::Null,
        &mock_capabilities_with_approvals(MockApprovals::pending()),
    )
    .await
    .expect("run");
    let slot = &out.output["nodes"]["review"];
    assert_eq!(slot["port"], "rejected");
    let item = &slot["items"][0]["json"];
    assert_eq!(item["approved"], json!(false));
    assert_eq!(item["timed_out"], json!(true));
    assert_eq!(item["edited"], json!(false));
    assert_eq!(item["decided_by"], Value::Null);
    assert!(
        item["comment"].as_str().is_some_and(|c| !c.is_empty()),
        "a timed-out rejection still carries a comment explaining why, got {item:?}"
    );
    assert_eq!(
        slot["decision"]["approved"],
        json!(false),
        "=nodes.review.decision.approved must resolve to false after a timeout, not be absent"
    );

    // drop: nothing emitted.
    let graph = wf(json!({
        "wait_mode": "poll",
        "poll_interval_ms": 1,
        "max_polls": 1,
        "on_timeout": "reject",
        "on_reject": "drop",
    }));
    let compiled = compile(&graph).expect("compile");
    let out = run(
        &compiled,
        Value::Null,
        &mock_capabilities_with_approvals(MockApprovals::pending()),
    )
    .await
    .expect("run");
    assert_eq!(out.output["nodes"]["review"]["items"], json!([]));

    // error: the node fails rather than silently continuing.
    let graph = wf(json!({
        "wait_mode": "poll",
        "poll_interval_ms": 1,
        "max_polls": 1,
        "on_timeout": "reject",
        "on_reject": "error",
    }));
    let compiled = compile(&graph).expect("compile");
    let err = run(
        &compiled,
        Value::Null,
        &mock_capabilities_with_approvals(MockApprovals::pending()),
    )
    .await
    .expect_err("on_timeout: reject with on_reject: error must fail the node");
    assert!(err.to_string().contains("on_reject"), "got {err}");
}

/// A decision that carries the reviewer's own edit (`payload`) drives
/// `subject` to that edit and marks `edited: true` — the feature this node
/// exists for. `decision_from_resume` and `decided_item` are exercised
/// separately elsewhere; this checks the two compose correctly, matching what
/// a `{"decision": {..., "payload": ...}}` resume value produces end to end.
#[test]
fn a_decision_with_a_payload_edits_the_subject() {
    let req = request("run-1:review");
    let resume_value = json!({
        "decision": {
            "node_id": "review",
            "approved": true,
            "decided_by": "ada",
            "payload": "https://example.com/edited",
        }
    });
    let decision = decision_from_resume(&resume_value, &req).expect("a decision");
    assert!(decision.approved);
    assert_eq!(decision.payload, Some(json!("https://example.com/edited")));

    let item = decided_item(
        &req,
        &decision,
        json!({ "url": "https://example.com/original" }),
    );
    let json = item.json;
    assert_eq!(json["approved"], json!(true));
    assert_eq!(json["subject"], "https://example.com/edited");
    assert_eq!(json["edited"], json!(true));
    assert_eq!(json["decided_by"], "ada");
    assert_eq!(
        json["input"],
        json!({ "url": "https://example.com/original" })
    );
}

/// When a resume (or a listed approval) settles the review, any provider card
/// opened by an earlier `decide` call must be withdrawn — otherwise the
/// provider's queue keeps a stale entry for a review the run already closed.
#[tokio::test]
async fn a_resume_decision_withdraws_the_provider_card() {
    let graph = wf(json!({ "title": "Publish this?" }));
    let compiled = compile(&graph).expect("compile");
    let provider = std::sync::Arc::new(MockApprovals::pending());
    let caps = crate::caps::Capabilities {
        approvals: Some(provider.clone()),
        ..mock_capabilities()
    };

    // First activation asks the provider and gets Pending, opening a card.
    let paused = run(&compiled, Value::Null, &caps).await.expect("run");
    assert_eq!(paused.pending_approvals, vec!["review".to_string()]);
    assert!(
        !provider.requested().is_empty(),
        "the provider must have been asked at least once"
    );
    assert!(provider.cancelled().is_empty(), "no reason to cancel yet");

    // A resume delivers the decision directly (bypassing the provider), so the
    // node must withdraw the provider's now-stale card rather than leave it.
    let resumed = resume(&compiled, Value::Null, vec!["review".to_string()], &caps)
        .await
        .expect("resume");
    assert_eq!(resumed.output["nodes"]["review"]["port"], "approved");
    assert!(
        !provider.cancelled().is_empty(),
        "the provider's card must be withdrawn once a resume settles the review"
    );
}

/// The host-seeded run id is what makes the default `request_id` work: unique
/// per run, and — crucially — the *same* across a resume, so a paused review
/// resolves the card already in front of a person.
#[tokio::test]
async fn a_host_seeded_run_id_derives_the_request_id_and_survives_a_resume() {
    use crate::caps::mock::MockApprovals;
    use crate::engine::{RunInput, resume};

    let graph = wf_raw(json!({ "title": "Publish this?" }));
    let compiled = compile(&graph).expect("compile");
    let provider = std::sync::Arc::new(MockApprovals::pending());
    let caps = crate::caps::Capabilities {
        approvals: Some(provider.clone()),
        ..mock_capabilities()
    };
    let input = || RunInput::new(json!({ "url": "https://example.com" })).with_run_id("run-7f3a");

    // Nobody has answered: the run pauses at the review.
    let paused = run(&compiled, input(), &caps).await.expect("run");
    assert_eq!(paused.pending_approvals, vec!["review".to_string()]);

    // Resuming the SAME run must address the SAME review, not open a second.
    let resumed = resume(&compiled, input(), vec!["review".to_string()], &caps)
        .await
        .expect("resume");
    assert_eq!(
        resumed.output["nodes"]["review"]["items"][0]["json"]["request_id"],
        "run-7f3a:review"
    );
    let ids = provider.requested();
    assert!(
        ids.iter().all(|id| id == "run-7f3a:review"),
        "every ask must name one review across the run and its resume, got {ids:?}"
    );
}

/// Two runs of the same graph must not share a review — that collision is
/// precisely what would let a later run inherit an earlier approval.
#[tokio::test]
async fn two_runs_of_one_graph_get_distinct_reviews() {
    use crate::engine::RunInput;

    let graph = wf_raw(json!({ "title": "Publish this?" }));
    let compiled = compile(&graph).expect("compile");
    // `mock_capabilities()` wires `MockApprovals::approving()` by default (see
    // its doc comment), so the review settles inline rather than pausing —
    // `items[0]` below is the approved decision, not a paused/null node.
    let caps = mock_capabilities();

    let first = run(
        &compiled,
        RunInput::new(json!({})).with_run_id("run-a"),
        &caps,
    )
    .await
    .expect("first run");
    let second = run(
        &compiled,
        RunInput::new(json!({})).with_run_id("run-b"),
        &caps,
    )
    .await
    .expect("second run");

    assert_eq!(
        first.output["nodes"]["review"]["items"][0]["json"]["request_id"],
        "run-a:review"
    );
    assert_eq!(
        second.output["nodes"]["review"]["items"][0]["json"]["request_id"],
        "run-b:review"
    );
}

/// THE self-approval bypass: a caller must not be able to approve their own
/// review by putting the node's id in the trigger payload they submit.
///
/// `engine::resume` writes the merged approvals into `run.trigger.approvals`
/// as well as the explicit channel, so it was tempting to read both. But the
/// trigger is whatever a caller handed to `engine::run` — on a webhook, that is
/// attacker-supplied — and a review its own subject can approve is not a
/// review. Only the explicit `RunInput::approvals` channel counts.
#[tokio::test]
async fn approvals_in_the_trigger_payload_cannot_self_approve_a_review() {
    let graph = wf_raw(json!({ "title": "Publish this?", "request_id": "review-1" }));
    let compiled = compile(&graph).expect("compile");
    let provider = std::sync::Arc::new(MockApprovals::pending());
    let caps = crate::caps::Capabilities {
        approvals: Some(provider.clone()),
        ..mock_capabilities()
    };

    // The attacker's payload names the review — by node id and by request id.
    let outcome = run(
        &compiled,
        json!({ "approvals": ["review", "review-1"] }),
        &caps,
    )
    .await
    .expect("run");

    assert_eq!(
        outcome.pending_approvals,
        vec!["review".to_string()],
        "a trigger-supplied approvals list must leave the review pending, not settle it"
    );
    assert_ne!(
        outcome.output["nodes"]["review"]["port"],
        json!("approved"),
        "the review must not have been approved by its own caller"
    );
}

/// The counterpart: the explicit host channel still settles the review, so
/// closing the bypass did not break the supported path.
#[tokio::test]
async fn the_explicit_approvals_channel_still_settles_a_review() {
    use crate::engine::RunInput;

    let graph = wf_raw(json!({ "title": "Publish this?", "request_id": "review-1" }));
    let compiled = compile(&graph).expect("compile");
    let caps = crate::caps::Capabilities {
        approvals: Some(std::sync::Arc::new(MockApprovals::pending())),
        ..mock_capabilities()
    };

    let outcome = run(
        &compiled,
        RunInput::new(json!({})).with_approvals(vec!["review".to_string()]),
        &caps,
    )
    .await
    .expect("run");

    assert_eq!(outcome.output["nodes"]["review"]["port"], "approved");
    assert!(outcome.pending_approvals.is_empty());
}

/// The self-approval bypass must not survive a **resume** either.
///
/// `merge_approvals` writes the merged list back into `run.trigger.approvals`
/// for the pre-existing `requires_approval` gate, and it used to seed its
/// starting set from that same key — which laundered a caller-written id into
/// the explicit channel on the first resume. So a payload the initial run
/// correctly refused was honoured on the next one. Provenance is kept separate
/// now: only ids a host actually authorised reach `run.approvals`.
#[tokio::test]
async fn a_trigger_supplied_approval_is_not_laundered_by_a_resume() {
    use crate::engine::resume;

    let graph = wf_raw(json!({ "title": "Publish this?", "request_id": "review-1" }));
    let compiled = compile(&graph).expect("compile");
    let caps = crate::caps::Capabilities {
        approvals: Some(std::sync::Arc::new(MockApprovals::pending())),
        ..mock_capabilities()
    };
    // The attacker names the review in the payload they submit.
    let attacker_trigger = json!({ "approvals": ["review", "review-1"] });

    let paused = run(&compiled, attacker_trigger.clone(), &caps)
        .await
        .expect("run");
    assert_eq!(paused.pending_approvals, vec!["review".to_string()]);

    // A resume that approves NOTHING must not promote the attacker's ids.
    let resumed = resume(&compiled, attacker_trigger, vec![], &caps)
        .await
        .expect("resume");
    assert_eq!(
        resumed.pending_approvals,
        vec!["review".to_string()],
        "a resume must not launder a trigger-supplied id into the trusted channel"
    );
    assert_ne!(resumed.output["nodes"]["review"]["port"], json!("approved"));
}
