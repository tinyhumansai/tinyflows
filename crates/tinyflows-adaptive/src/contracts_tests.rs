use super::*;

#[test]
fn two_errands_in_one_episode_are_visibly_the_same_attempt() {
    // Unlike two authored graphs, which may genuinely differ and are told
    // apart by their fingerprints. An errand carries no plan to differ in,
    // so the constant signature is what puts it in the exclusion list and
    // stops an episode spending its budget on identical single turns.
    let first = Approach::Errand {
        why: "one turn of work".into(),
    };
    let second = Approach::Errand {
        why: "still one turn, honestly".into(),
    };
    assert_eq!(first.signature(), "errand");
    assert_eq!(first.signature(), second.signature());
}

#[test]
fn an_errand_cannot_collide_with_a_stored_workflow_called_errand() {
    // The namespacing that makes the constant safe: `selected:` prefixes
    // every workflow id, so no shelf entry can occupy the errand slot.
    let selected = Approach::Selected {
        workflow_id: "errand".into(),
        why: String::new(),
    };
    assert_eq!(selected.signature(), "selected:errand");
    assert_ne!(
        selected.signature(),
        Approach::Errand { why: String::new() }.signature()
    );
}

#[test]
fn an_unrecognised_blocker_is_continuable_rather_than_terminal() {
    // `goal_not_meet` — one letter — used to end a run at attempt 3 of 12.
    assert_eq!(Blocker::parse("goal_not_meet"), Blocker::GoalNotMet);
    assert_eq!(Blocker::parse("something new"), Blocker::GoalNotMet);
    assert!(Blocker::parse("nonsense").continuable());
}

#[test]
fn the_terminal_blockers_stop_a_run() {
    assert!(!Blocker::MissingEvidence.continuable());
    assert!(!Blocker::NeedsInput.continuable());
    assert!(!Blocker::ExternalWait.continuable());
}

#[test]
fn an_empty_blocker_reads_as_no_blocker() {
    assert_eq!(Blocker::parse(""), Blocker::None);
}

fn verdict(satisfied: bool, blocker: Blocker) -> Verdict {
    Verdict {
        satisfied,
        blocker,
        gap: String::new(),
        attributed_to: String::new(),
        evidence: String::new(),
        advanced: false,
    }
}

#[test]
fn the_stall_rule_does_not_apply_before_min_attempts() {
    // Early attempts look flat while a run is still orienting.
    let budget = Budget::default();
    let v = verdict(false, Blocker::GoalNotMet);
    assert!(
        v.should_retry(1, 5, &budget),
        "attempt 1 must not be stalled out"
    );
    assert!(v.should_retry(2, 5, &budget));
    assert!(
        !v.should_retry(3, 2, &budget),
        "past min_attempts the rule bites"
    );
}

#[test]
fn a_converging_run_is_not_killed_by_the_counter() {
    let budget = Budget::default();
    let mut v = verdict(false, Blocker::GoalNotMet);
    v.advanced = true;
    // `stalled` is reset by the caller on every advancing attempt, so a run
    // that keeps advancing never accumulates one.
    assert!(v.should_retry(9, 0, &budget));
}

#[test]
fn a_satisfied_verdict_never_retries() {
    assert!(!verdict(true, Blocker::None).should_retry(1, 0, &Budget::default()));
}

#[test]
fn a_terminal_blocker_stops_even_with_budget_left() {
    let v = verdict(false, Blocker::NeedsInput);
    assert!(!v.should_retry(1, 0, &Budget::default()));
}

#[test]
fn the_attempt_ceiling_is_still_a_backstop() {
    let budget = Budget::default();
    let mut v = verdict(false, Blocker::GoalNotMet);
    v.advanced = true;
    assert!(!v.should_retry(12, 0, &budget));
}

#[test]
fn a_signature_names_the_kind_of_attempt_not_the_task() {
    let selected = Approach::Selected {
        workflow_id: "pr-review".into(),
        why: "matches".into(),
    };
    assert_eq!(selected.signature(), "selected:pr-review");
}

#[test]
fn two_authored_attempts_are_told_apart_by_their_graph() {
    // Before the fingerprint every authoring attempt signed as "authored",
    // `tried()` folded them to one entry, and attempt four could re-author
    // attempt two word for word with nothing to notice.
    let first = Approach::Authored {
        why: "nothing fitted".into(),
        fingerprint: "1111111".into(),
    };
    let second = Approach::Authored {
        why: "still nothing fitted".into(),
        fingerprint: "2222222".into(),
    };
    assert_ne!(first.signature(), second.signature());
    assert_eq!(first.signature(), "authored:1111111");
}

#[test]
fn the_same_graph_authored_twice_signs_the_same_and_is_caught() {
    // The other half: a differently-worded `why` around an identical graph
    // is the same attempt, and must read as the repeat it is.
    let first = Approach::Authored {
        why: "nothing fitted".into(),
        fingerprint: "1111111".into(),
    };
    let again = Approach::Authored {
        why: "a fresh idea, honestly".into(),
        fingerprint: "1111111".into(),
    };
    assert_eq!(first.signature(), again.signature());
}

#[test]
fn a_verdict_round_trips_through_json() {
    // The judge answers in JSON and the ledger stores JSON; a field lost in
    // either direction is one that works in a test and never in a run.
    let v = verdict(false, Blocker::Unverified);
    let back: Verdict = serde_json::from_str(&serde_json::to_string(&v).unwrap()).unwrap();
    assert_eq!(back.blocker, Blocker::Unverified);
    assert!(!back.advanced);
}

#[test]
fn advanced_defaults_to_true_when_a_model_omits_it() {
    // Absent must not read as "made no progress" — that would stall a run
    // for a field the model simply did not write.
    let v: Verdict =
        serde_json::from_str(r#"{"satisfied":false,"blocker":"goal_not_met"}"#).unwrap();
    assert!(v.advanced);
}
