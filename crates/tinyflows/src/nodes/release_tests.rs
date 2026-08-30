use super::*;
use serde_json::json;

#[test]
fn all_waits_for_everything_then_times_out_rather_than_emitting_partially() {
    let policy = ReleasePolicy::All;
    assert_eq!(policy.evaluate(2, 3, false), Release::Wait);
    assert_eq!(policy.evaluate(3, 3, false), Release::Emit);
    assert_eq!(
        policy.evaluate(2, 3, true),
        Release::Timeout,
        "`all` must never emit a partial result"
    );
}

#[test]
fn any_goes_on_the_first_arrival() {
    assert_eq!(ReleasePolicy::Any.evaluate(0, 5, false), Release::Wait);
    assert_eq!(ReleasePolicy::Any.evaluate(1, 5, false), Release::Emit);
}

#[test]
fn first_n_and_quorum_go_at_n() {
    for policy in [ReleasePolicy::FirstN(3), ReleasePolicy::Quorum(3)] {
        assert_eq!(policy.evaluate(2, 5, false), Release::Wait);
        assert_eq!(policy.evaluate(3, 5, false), Release::Emit);
    }
}

/// A partial release must never hand downstream fewer results than the
/// policy promised — the property worth pinning, since `n` is the whole
/// contract of `first_n`/`quorum`.
#[test]
fn first_n_never_emits_with_fewer_than_n() {
    let policy = ReleasePolicy::FirstN(3);
    for arrived in 0..3 {
        assert_ne!(
            policy.evaluate(arrived, 10, true),
            Release::Emit,
            "emitted with only {arrived} of the promised 3"
        );
    }
}

/// An `n` larger than the number of things being waited for is an authoring
/// mistake that must not become a hang.
#[test]
fn a_threshold_above_the_expected_count_is_clamped() {
    assert_eq!(
        ReleasePolicy::Quorum(5).evaluate(3, 3, false),
        Release::Emit
    );
    assert_eq!(ReleasePolicy::Any.evaluate(0, 0, false), Release::Emit);
}

#[test]
fn timeout_partial_settles_for_what_arrived_only_once_the_budget_is_spent() {
    let policy = ReleasePolicy::TimeoutPartial;
    assert_eq!(policy.evaluate(1, 3, false), Release::Wait);
    assert_eq!(policy.evaluate(1, 3, true), Release::Emit);
}

#[test]
fn config_parses_every_policy() {
    let parse = |value: Value| ReleasePolicy::from_config(&value, "g");
    assert_eq!(parse(json!({})).unwrap(), ReleasePolicy::All);
    assert_eq!(
        parse(json!({ "release": "all" })).unwrap(),
        ReleasePolicy::All
    );
    assert_eq!(
        parse(json!({ "release": "any" })).unwrap(),
        ReleasePolicy::Any
    );
    assert_eq!(
        parse(json!({ "release": "first_n", "n": 2 })).unwrap(),
        ReleasePolicy::FirstN(2)
    );
    assert_eq!(
        parse(json!({ "release": "quorum", "n": 4 })).unwrap(),
        ReleasePolicy::Quorum(4)
    );
    assert_eq!(
        parse(json!({ "release": "timeout_partial" })).unwrap(),
        ReleasePolicy::TimeoutPartial
    );
}

/// Failing closed matters here: defaulting a missing `n` to 1 would turn a
/// declared quorum into `any` and release far too early.
#[test]
fn a_missing_or_zero_n_is_refused_rather_than_defaulted() {
    for config in [
        json!({ "release": "first_n" }),
        json!({ "release": "quorum", "n": 0 }),
        json!({ "release": "quorum", "n": "three" }),
    ] {
        assert!(
            ReleasePolicy::from_config(&config, "g").is_err(),
            "config {config} should be refused rather than given a default `n`"
        );
    }
}

#[test]
fn an_unknown_policy_is_refused_and_the_message_lists_the_valid_ones() {
    let err = ReleasePolicy::from_config(&json!({ "release": "eventually" }), "g")
        .expect_err("unknown policy");
    let message = err.to_string();
    assert!(
        message.contains("eventually"),
        "names the bad value: {message}"
    );
    assert!(
        message.contains("quorum"),
        "lists the valid ones: {message}"
    );
}
