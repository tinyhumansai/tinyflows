//! Testing the measure, because a broken slope would let a null result look
//! like a win — and a null result that looks like a win is worse than no eval,
//! since it ends the work that would have found the truth.

use super::{Episode, Experiment, Forgetful, LEARNING_OFF, LEARNING_ON, Outcome, Series, Verdict};

fn ep(index: usize, attempts: u32) -> Episode {
    Episode {
        task: format!("t{index}"),
        family: "euler".to_string(),
        attempts,
        satisfied: true,
        verified: None,
        cost_usd: 0.0,
        used_workflow: false,
        lessons_applied: 0,
    }
}

fn series(label: &str, attempts: &[u32]) -> Series {
    Series {
        label: label.to_string(),
        episodes: attempts
            .iter()
            .enumerate()
            .map(|(i, a)| ep(i, *a))
            .collect(),
    }
}

fn experiment(on: &[u32], off: &[u32]) -> Experiment {
    let mut experiment = Experiment::new("euler");
    for (i, a) in on.iter().enumerate() {
        experiment.record(LEARNING_ON, ep(i, *a));
    }
    for (i, a) in off.iter().enumerate() {
        experiment.record(LEARNING_OFF, ep(i, *a));
    }
    experiment
}

#[test]
fn a_converging_series_has_a_negative_slope() {
    assert!(
        series("learning_on", &[5, 4, 3, 2, 1])
            .trend()
            .expect("a fit")
            < 0.0
    );
}

#[test]
fn a_flat_series_has_no_slope() {
    // A retry loop wearing a learning costume.
    assert!(
        series("learning_off", &[3, 3, 3, 3])
            .trend()
            .expect("a fit")
            .abs()
            < f64::EPSILON
    );
}

#[test]
fn a_worsening_series_has_a_positive_slope() {
    assert!(series("bad", &[1, 2, 3, 4]).trend().expect("a fit") > 0.0);
}

#[test]
fn a_single_episode_cannot_show_a_trend() {
    // Two points make a line; one makes an anecdote.
    assert_eq!(series("x", &[4]).trend(), None, "one point is an anecdote");
    assert_eq!(series("x", &[]).trend(), None);
}

#[test]
fn unsolved_episodes_are_excluded_from_the_slope() {
    // Attempts on a task that never succeeded say nothing about convergence —
    // that number is a budget being spent, not a distance being closed.
    let mut mixed = series("x", &[5, 9, 1]);
    mixed.episodes[1].satisfied = false;
    assert!(mixed.trend().expect("a fit") < 0.0);
    assert_eq!(mixed.solved(), 2);
}

#[test]
fn a_failure_is_a_gap_in_the_sequence_not_a_shift_of_what_follows() {
    // A win keeps the position it actually happened at, so the fit measures
    // convergence *per episode of the family* — including the episodes that
    // bought nothing.
    //
    // Which makes a failure count against the arm, and it is worth being
    // explicit about the direction because the intuition runs the other way:
    // preserving the gap makes the slope SHALLOWER, not steeper. Six attempts
    // falling to two over four episodes is slower learning than the same fall
    // over three, and re-indexing the wins 0,1,2 would report the faster
    // number for the worse run.
    let mut with_gap = series("x", &[6, 9, 4, 2]);
    with_gap.episodes[1].satisfied = false;
    let without_the_wasted_episode = series("x", &[6, 4, 2]);

    let gapped = with_gap.trend().expect("a fit");
    let dense = without_the_wasted_episode.trend().expect("a fit");
    assert!(gapped < 0.0, "still converging");
    assert!(
        gapped > dense,
        "the wasted episode must cost the arm: {gapped} should be shallower than {dense}"
    );
}

#[test]
fn the_claim_is_a_comparison_not_an_absolute() {
    assert!(experiment(&[5, 4, 2, 1], &[4, 4, 4, 4]).learning_helps());
}

#[test]
fn learning_does_not_win_on_a_steeper_slope_alone() {
    // A curve that bends but never converges is not a win: twenty attempts
    // falling to ten loses to a flat two.
    assert!(!experiment(&[20, 15, 10], &[2, 2, 2]).learning_helps());
}

#[test]
fn disjoint_tasks_cannot_distinguish_the_arms() {
    // The warning that motivates the whole eval design. With no repetition
    // both arms look identical, and a success-rate metric would report the tie
    // as evidence of something.
    assert!(!experiment(&[3, 3, 3], &[3, 3, 3]).learning_helps());
}

#[test]
fn a_missing_arm_never_claims_a_win() {
    let mut one_arm = Experiment::new("euler");
    for (i, a) in [5, 1].iter().enumerate() {
        one_arm.record(LEARNING_ON, ep(i, *a));
    }
    assert!(!one_arm.learning_helps(), "one arm compares with nothing");
    assert!(!Experiment::new("euler").learning_helps());
}

#[test]
fn cost_per_solve_counts_failed_episodes_against_the_arm() {
    // Spending on failures is real spending, and an arm that burns three
    // episodes to win one has not won cheaply.
    let mut arm = series("x", &[1, 9]);
    arm.episodes[0].cost_usd = 1.0;
    arm.episodes[1].cost_usd = 3.0;
    arm.episodes[1].satisfied = false;
    assert!((arm.cost_per_solve() - 4.0).abs() < 1e-9);
}

#[test]
fn an_arm_that_never_solved_reports_no_cost_per_solve_rather_than_infinity() {
    let mut arm = series("x", &[9, 9]);
    for episode in &mut arm.episodes {
        episode.satisfied = false;
        episode.cost_usd = 2.0;
    }
    assert_eq!(arm.cost_per_solve(), 0.0);
    assert_eq!(
        arm.mean_attempts(),
        None,
        "no wins is not an attempt count of zero — zero would beat every real arm"
    );
}

#[test]
fn workflow_hit_rate_tracks_whether_the_store_is_being_used() {
    let mut arm = series("x", &[2, 1]);
    arm.episodes[1].used_workflow = true;
    assert!((arm.workflow_hit_rate() - 0.5).abs() < 1e-9);
    assert_eq!(Series::new("empty").workflow_hit_rate(), 0.0);
}

#[test]
fn an_answer_the_judge_accepted_and_the_world_refuted_is_not_a_solve() {
    // The state that must never be folded into success: everything downstream
    // believes a run the judge passed, so a wrong one is the most expensive
    // thing to be blind to.
    let wrong = ep(0, 1).checked(false);
    assert_eq!(wrong.outcome(), Outcome::Wrong);
    assert!(!wrong.solved(), "the judge does not get the last word");

    let mut arm = Series::new("x");
    arm.episodes.push(wrong);
    assert_eq!(arm.solved(), 0);
    assert_eq!(arm.trend(), None, "nothing solved, nothing to fit");
}

#[test]
fn right_but_refused_is_reported_apart_from_failing() {
    // A judging defect, not a solving one. Counting it as a plain failure
    // hides which half of the loop to go and fix.
    let mut refused = ep(0, 3);
    refused.satisfied = false;
    let refused = refused.checked(true);
    assert_eq!(refused.outcome(), Outcome::Refused);
    assert!(!refused.solved());

    let mut failed = ep(1, 3);
    failed.satisfied = false;
    assert_eq!(failed.outcome(), Outcome::Failed);

    let arm = Series {
        label: "x".to_string(),
        episodes: vec![refused, failed],
    };
    let counts = arm.outcomes();
    assert_eq!(counts["refused"], 1);
    assert_eq!(counts["failed"], 1);
}

#[test]
fn an_unchecked_episode_is_taken_at_the_judges_word_but_says_so() {
    // `None` is not `Some(true)`: it means the eval could not check. Both
    // count as solved; only the report can tell them apart, which is the point
    // of keeping the field three-valued.
    let unchecked = ep(0, 2);
    assert_eq!(unchecked.verified, None);
    assert!(unchecked.solved());
    assert_eq!(unchecked.outcome(), Outcome::Solved);
}

#[test]
fn the_report_carries_every_claim() {
    let mut experiment = experiment(&[4, 2], &[3, 3]);
    experiment.arms.get_mut(LEARNING_ON).expect("arm").episodes[1].used_workflow = true;
    let report = experiment.report();

    assert_eq!(report["family"], "euler");
    assert_eq!(report["learning_helps"], true);
    let on = &report["arms"][LEARNING_ON];
    for key in [
        "solved",
        "episodes",
        "mean_attempts",
        "slope",
        "cost_per_solve",
        "workflow_hit_rate",
        "outcomes",
    ] {
        assert!(!on[key].is_null(), "report is missing {key}: {on}");
    }
    assert!(on["slope"].as_f64().expect("a number") < 0.0);
    assert!((on["workflow_hit_rate"].as_f64().expect("a number") - 0.5).abs() < 1e-9);
}

#[test]
fn solving_what_the_other_arm_cannot_is_the_largest_win_available() {
    // The case that exposed the defect this verdict exists for. A live run
    // produced exactly this — treatment solved four of five, control solved
    // NONE — and the first version reported no evidence, because both slopes
    // read `0.0`: one meaning "flat", the other meaning "one point is not a
    // line". Going from never to once is the biggest fall in
    // attempts-to-success there is.
    let mut experiment = Experiment::new("euler");
    for (i, attempts) in [4, 1, 1, 1, 1].iter().enumerate() {
        let mut episode = ep(i, *attempts);
        episode.satisfied = i > 0;
        experiment.record(LEARNING_ON, episode);
    }
    for i in 0..5 {
        let mut episode = ep(i, 4);
        episode.satisfied = false;
        experiment.record(LEARNING_OFF, episode);
    }

    assert_eq!(
        experiment.compare(LEARNING_ON, LEARNING_OFF),
        Verdict::SolvedMore
    );
    assert!(experiment.learning_helps());
    assert_eq!(experiment.report()["verdict"], "solved_more");
}

#[test]
fn solving_more_does_not_win_if_each_solve_costs_more() {
    // The guard that keeps "solved more" from becoming a success-rate metric
    // in disguise: an arm that solves twice as many at ten attempts each has
    // not learned anything the control's two-attempt solves did not know.
    let mut experiment = Experiment::new("euler");
    for (i, attempts) in [10, 10, 10, 10].iter().enumerate() {
        experiment.record(LEARNING_ON, ep(i, *attempts));
    }
    for i in 0..4 {
        let mut episode = ep(i, 2);
        episode.satisfied = i < 2;
        experiment.record(LEARNING_OFF, episode);
    }

    assert_eq!(
        experiment.compare(LEARNING_ON, LEARNING_OFF),
        Verdict::NoEvidence,
        "more solves at a worse attempt cost is not learning"
    );
}

#[test]
fn a_converging_arm_reports_which_test_it_won_on() {
    let experiment = experiment(&[5, 4, 2, 1], &[4, 4, 4, 4]);
    assert_eq!(
        experiment.compare(LEARNING_ON, LEARNING_OFF),
        Verdict::Converged
    );
    assert_eq!(experiment.report()["verdict"], "converged");
}

#[test]
fn a_tie_reports_no_evidence_by_name() {
    let experiment = experiment(&[3, 3, 3], &[3, 3, 3]);
    assert_eq!(
        experiment.compare(LEARNING_ON, LEARNING_OFF),
        Verdict::NoEvidence
    );
    assert_eq!(experiment.report()["verdict"], "no_evidence");
}

#[test]
fn an_arm_with_no_wins_reports_null_rather_than_a_flattering_zero() {
    // What the report says has to survive being read by somebody who did not
    // write it: `"slope": 0.0` on an arm that solved nothing reads as "no
    // learning observed", which is a stronger claim than the data supports.
    let mut experiment = Experiment::new("euler");
    for i in 0..3 {
        let mut episode = ep(i, 4);
        episode.satisfied = false;
        experiment.record(LEARNING_OFF, episode);
    }
    let arm = &experiment.report()["arms"][LEARNING_OFF];
    assert!(arm["slope"].is_null(), "{arm}");
    assert!(arm["mean_attempts"].is_null(), "{arm}");
    assert_eq!(arm["solved"], 0);
}

// ---------------------------------------------------------------------------
// The control arm.
// ---------------------------------------------------------------------------

use crate::ledger::{Ledger, LedgerRow, Lesson, LessonKind, memory::MemoryLedger};

fn row(episode: &str, workflow: Option<&str>) -> LedgerRow {
    LedgerRow {
        id: String::new(),
        episode: episode.to_string(),
        attempt: 1,
        approach_sig: "selected:sweep".to_string(),
        approach_desc: "it matched".to_string(),
        workflow_id: workflow.map(ToString::to_string),
        outcome: "satisfied".to_string(),
        cause: String::new(),
        cost_usd: 0.5,
        at: "2026-01-01T00:00:00Z".to_string(),
        satisfied: true,
        advanced: true,
    }
}

#[tokio::test]
async fn the_control_arm_still_sees_its_own_episode() {
    // Not a loop with its ledger removed. The exclusion list and the stall
    // counter live in these rows, and an arm that could not plan would lose
    // the experiment for a reason that has nothing to do with learning.
    let ledger = Forgetful::new(MemoryLedger::new());
    ledger
        .append(&row("ep-1", Some("sweep")))
        .await
        .expect("append");

    let rows = ledger.rows("ep-1").await.expect("rows");
    assert_eq!(rows.len(), 1, "this episode's attempts are still visible");
    assert_eq!(rows[0].workflow_id.as_deref(), Some("sweep"));
    assert_eq!(
        ledger.tried("ep-1").await.expect("tried"),
        vec!["selected:sweep".to_string()],
        "so attempt two does not repeat attempt one"
    );
}

#[tokio::test]
async fn the_control_arm_is_told_nothing_that_outlived_another_episode() {
    let ledger = Forgetful::new(MemoryLedger::new());
    let id = ledger
        .promote(
            &Lesson {
                id: String::new(),
                kind: LessonKind::Strategy,
                trigger: "a CPU-bound scan over ~1M items".to_string(),
                mechanism: "the obvious enumeration is quadratic".to_string(),
                claim: "cache the sub-results and build upward".to_string(),
                applied: 0,
                helped: 0,
                scope_key: None,
            },
            &[],
        )
        .await
        .expect("promote");
    ledger.score_workflow("sweep", true).await.expect("score");

    // Written, and then unreadable — which is exactly the variable under test.
    assert!(
        ledger.lessons(None).await.expect("lessons").is_empty(),
        "a planner in this arm recalls nothing from other episodes"
    );
    assert!(ledger.evidence(&id).await.expect("evidence").is_empty());
    let score = ledger.workflow_score("sweep").await.expect("score");
    assert_eq!(
        (score.applied, score.helped),
        (0, 0),
        "and weighs no record it did not earn this episode"
    );

    // The writes did land underneath, so the arm paid for consolidation just
    // as the treatment arm did — `cost_per_solve` must not favour it for
    // skipping work rather than for learning.
    assert_eq!(ledger.inner().lessons(None).await.expect("under").len(), 1);
}

#[tokio::test]
async fn the_control_arm_sees_no_repair_lineage() {
    // A variant is knowledge from a previous episode as much as a lesson is,
    // and `collapse_families` reads lineage to decide what the planner is
    // offered.
    let ledger = Forgetful::new(MemoryLedger::new());
    ledger
        .link_variant("sweep", "sweep-fix")
        .await
        .expect("link");
    assert_eq!(ledger.parent_of("sweep-fix").await.expect("parent"), None);
    assert!(
        ledger
            .children_of("sweep")
            .await
            .expect("children")
            .is_empty()
    );
    assert_eq!(
        ledger.inner().parent_of("sweep-fix").await.expect("under"),
        Some("sweep".to_string()),
        "recorded underneath, just not offered"
    );
}
