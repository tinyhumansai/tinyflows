//! One suite every [`Ledger`] backend must pass.
//!
//! Two backends with separate test files drift: sqlite gets a case, Mongo does
//! not, and the difference surfaces in production as "it worked locally". So
//! the cases live here, take `&dyn Ledger`, and each backend's own tests are a
//! four-line call into this module.
//!
//! Compiled always, not behind `cfg(test)`, so a host writing its own backend
//! can run the same suite against it.

use super::{Episode, EpisodeStatus, Ledger, LedgerRow, Lesson, LessonKind};

/// A row with the fields a test does not care about filled in.
#[must_use]
pub fn row(episode: &str, attempt: u32, sig: &str) -> LedgerRow {
    LedgerRow {
        id: String::new(),
        episode: episode.to_string(),
        attempt,
        approach_sig: sig.to_string(),
        approach_desc: format!("attempt {attempt} via {sig}"),
        workflow_id: None,
        outcome: String::new(),
        cause: String::new(),
        cost_usd: 0.0,
        at: format!("2026-01-01T00:00:{attempt:02}Z"),
        satisfied: false,
        advanced: false,
    }
}

/// A lesson with a trigger that describes a class rather than an instance.
#[must_use]
pub fn lesson(trigger: &str) -> Lesson {
    Lesson {
        id: String::new(),
        kind: LessonKind::Constraint,
        trigger: trigger.to_string(),
        mechanism: "because the API caps a page at 100".to_string(),
        claim: "page the listing rather than raising per_page".to_string(),
        applied: 0,
        helped: 0,
        scope_key: None,
    }
}

/// Run every case against `store`. Panics with a named assertion on failure,
/// so a backend's own test is one line and the failure still says what broke.
///
/// # Panics
/// On any conformance failure, or if the backend errors on a call the contract
/// says must succeed.
pub async fn run_all(store: &dyn Ledger) {
    appended_rows_come_back_in_order(store).await;
    an_episode_sees_only_its_own_rows(store).await;
    tried_is_the_deduplicated_exclusion_list(store).await;
    an_unknown_episode_is_empty_not_an_error(store).await;
    a_lesson_round_trips_with_its_evidence(store).await;
    lessons_filter_by_kind(store).await;
    scoring_a_lesson_moves_applied_always_and_helped_conditionally(store).await;
    a_workflow_nobody_has_run_scores_zero_rather_than_erroring(store).await;
    workflow_scores_accumulate(store).await;
    run_lineage(store).await;
    run_episodes(store).await;
    run_transcripts(store).await;
}

async fn appended_rows_come_back_in_order(store: &dyn Ledger) {
    let ep = "ep-order";
    for n in 1..=3 {
        store.append(&row(ep, n, "authored")).await.expect("append");
    }
    let got = store.rows(ep).await.expect("rows");
    assert_eq!(
        got.iter().map(|r| r.attempt).collect::<Vec<_>>(),
        vec![1, 2, 3],
        "rows must read oldest first — a ledger read backwards makes every gap analysis wrong"
    );
    assert!(!got[0].id.is_empty(), "append must assign an id");
}

async fn an_episode_sees_only_its_own_rows(store: &dyn Ledger) {
    store
        .append(&row("ep-a", 1, "authored"))
        .await
        .expect("append");
    store
        .append(&row("ep-b", 1, "authored"))
        .await
        .expect("append");
    assert_eq!(store.rows("ep-a").await.expect("rows").len(), 1);
    assert_eq!(store.rows("ep-b").await.expect("rows").len(), 1);
}

async fn tried_is_the_deduplicated_exclusion_list(store: &dyn Ledger) {
    let ep = "ep-tried";
    store
        .append(&row(ep, 1, "selected:pr-review"))
        .await
        .expect("append");
    store.append(&row(ep, 2, "authored")).await.expect("append");
    store.append(&row(ep, 3, "authored")).await.expect("append");

    let tried = store.tried(ep).await.expect("tried");
    assert_eq!(
        tried,
        vec!["selected:pr-review".to_string(), "authored".to_string()],
        "each signature once, in the order first spent"
    );
}

async fn an_unknown_episode_is_empty_not_an_error(store: &dyn Ledger) {
    // A first-time goal must read its (absent) history without failing, or
    // every episode's first attempt errors.
    assert!(store.rows("never-seen").await.expect("rows").is_empty());
    assert!(store.tried("never-seen").await.expect("tried").is_empty());
}

async fn a_lesson_round_trips_with_its_evidence(store: &dyn Ledger) {
    let ep = "ep-lesson";
    let a = store.append(&row(ep, 1, "authored")).await.expect("append");
    let b = store.append(&row(ep, 2, "authored")).await.expect("append");

    let id = store
        .promote(
            &lesson("a paginated listing API with a hard per-page cap"),
            &[a.clone(), b.clone()],
        )
        .await
        .expect("promote");
    assert!(!id.is_empty());

    let cited = store.evidence(&id).await.expect("evidence");
    let mut ids: Vec<String> = cited.into_iter().map(|r| r.id).collect();
    ids.sort();
    let mut want = vec![a, b];
    want.sort();
    assert_eq!(
        ids, want,
        "a lesson must be able to show the rows behind it"
    );
}

async fn lessons_filter_by_kind(store: &dyn Ledger) {
    let mut strategy = lesson("a wide fan-out over independent items");
    strategy.kind = LessonKind::Strategy;
    store.promote(&strategy, &[]).await.expect("promote");

    let only = store
        .lessons(Some(LessonKind::Strategy))
        .await
        .expect("lessons");
    assert!(!only.is_empty());
    assert!(only.iter().all(|l| l.kind == LessonKind::Strategy));

    let all = store.lessons(None).await.expect("lessons");
    assert!(all.len() >= only.len(), "None must not filter");
}

async fn scoring_a_lesson_moves_applied_always_and_helped_conditionally(store: &dyn Ledger) {
    let id = store
        .promote(&lesson("a scoring probe"), &[])
        .await
        .expect("promote");

    store.score_lesson(&id, true).await.expect("score");
    store.score_lesson(&id, false).await.expect("score");

    let found = store
        .lessons(None)
        .await
        .expect("lessons")
        .into_iter()
        .find(|l| l.id == id)
        .expect("the lesson just promoted");

    assert_eq!(found.applied, 2, "shown twice");
    assert_eq!(found.helped, 1, "only one of those runs was satisfied");
}

async fn a_workflow_nobody_has_run_scores_zero_rather_than_erroring(store: &dyn Ledger) {
    let score = store.workflow_score("never-run").await.expect("score");
    assert_eq!(score.applied, 0);
    assert_eq!(score.helped, 0);
}

async fn workflow_scores_accumulate(store: &dyn Ledger) {
    let id = "wf-accumulate";
    store.score_workflow(id, true).await.expect("score");
    store.score_workflow(id, true).await.expect("score");
    store.score_workflow(id, false).await.expect("score");

    let score = store.workflow_score(id).await.expect("score");
    assert_eq!(score.applied, 3);
    assert_eq!(
        score.helped, 2,
        "2 of 3 — the evidence a promotion gate reads"
    );
}

include!("conformance/tenants.rs");
include!("conformance/lineage.rs");
include!("conformance/episodes.rs");
include!("conformance/transcripts.rs");
