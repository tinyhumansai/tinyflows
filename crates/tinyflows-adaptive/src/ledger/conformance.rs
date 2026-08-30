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

/// Run every tenant-isolation case.
///
/// Separate from [`run_all`] because it needs three handles onto **one**
/// store — global, and two tenants — and how a backend makes a scoped handle
/// is its own business (`for_tenant` on both that ship). A backend that does
/// not support scoping simply does not call this.
///
/// # Panics
/// On any isolation failure. Each one is a leak of one tenant's knowledge into
/// another's prompt, so none of them is a soft assertion.
pub async fn run_tenants(global: &dyn Ledger, a: &dyn Ledger, b: &dyn Ledger) {
    assert_eq!(global.scope(), None, "the global handle must be unscoped");
    assert!(a.scope().is_some() && b.scope().is_some(), "both scoped");
    assert_ne!(a.scope(), b.scope(), "two different tenants");

    a_tenants_lesson_is_invisible_to_another(a, b).await;
    a_global_lesson_is_visible_to_every_tenant(global, a, b).await;
    promote_stamps_the_handle_not_the_argument(a).await;
    workflow_scores_do_not_bleed_between_tenants(a, b).await;
    a_tenant_writing_does_not_move_the_global_score(global, a).await;
    an_episode_id_alone_does_not_reach_another_tenants_attempts(a, b).await;
    naming_another_tenants_lesson_id_does_not_move_its_score(global, a, b).await;
}

async fn naming_another_tenants_lesson_id_does_not_move_its_score(
    global: &dyn Ledger,
    a: &dyn Ledger,
    b: &dyn Ledger,
) {
    // The ids reaching `score_lesson` come from model output (corroboration),
    // so this is a hole a prompt injection walks through if the backend
    // updates by id alone.
    let private = a
        .promote(&lesson("a private class of situation"), &[])
        .await
        .expect("promote");
    b.score_lesson(&private, true)
        .await
        .expect("no-op, not error");
    let untouched = a
        .lessons(None)
        .await
        .expect("lessons")
        .into_iter()
        .find(|l| l.id == private)
        .expect("still there");
    assert_eq!(
        (untouched.applied, untouched.helped),
        (0, 0),
        "tenant {:?} moved tenant {:?}'s score by naming its id",
        b.scope(),
        a.scope()
    );

    // A global lesson is visible to every tenant, so scoring it is legitimate.
    let shared = global
        .promote(&lesson("a class anyone can hit"), &[])
        .await
        .expect("promote");
    b.score_lesson(&shared, true).await.expect("score");
    let moved = b
        .lessons(None)
        .await
        .expect("lessons")
        .into_iter()
        .find(|l| l.id == shared)
        .expect("visible");
    assert_eq!((moved.applied, moved.helped), (1, 1));
}

async fn an_episode_id_alone_does_not_reach_another_tenants_attempts(
    a: &dyn Ledger,
    b: &dyn Ledger,
) {
    // An episode id is opaque and a service may hand one straight through from
    // a request path. Being keyed by episode is not isolation — guessing an id
    // would be enough — so the rows carry the bucket too.
    a.append(&row("ep-secret", 1, "authored:aaa"))
        .await
        .expect("append");
    assert_eq!(a.rows("ep-secret").await.expect("rows").len(), 1);
    assert!(
        b.rows("ep-secret").await.expect("rows").is_empty(),
        "tenant {:?} read tenant {:?}'s attempts by knowing the episode id",
        b.scope(),
        a.scope()
    );
}

async fn a_tenants_lesson_is_invisible_to_another(a: &dyn Ledger, b: &dyn Ledger) {
    let mut mine = lesson("a private class of task");
    mine.claim = "names an internal repository path".into();
    let id = a.promote(&mine, &[]).await.expect("promote");

    let seen_by_a = a.lessons(None).await.expect("lessons");
    assert!(
        seen_by_a.iter().any(|l| l.id == id),
        "a tenant must see its own lesson"
    );

    let seen_by_b = b.lessons(None).await.expect("lessons");
    assert!(
        !seen_by_b.iter().any(|l| l.id == id),
        "tenant {:?} can read tenant {:?}'s lesson — this is the leak the scope exists to stop",
        b.scope(),
        a.scope()
    );
}

async fn a_global_lesson_is_visible_to_every_tenant(
    global: &dyn Ledger,
    a: &dyn Ledger,
    b: &dyn Ledger,
) {
    let id = global
        .promote(&lesson("a class of task anyone can hit"), &[])
        .await
        .expect("promote");
    for tenant in [a, b] {
        let seen = tenant.lessons(None).await.expect("lessons");
        assert!(
            seen.iter().any(|l| l.id == id),
            "tenant {:?} cannot see a global lesson",
            tenant.scope()
        );
    }
}

async fn promote_stamps_the_handle_not_the_argument(a: &dyn Ledger) {
    // A caller — or a model whose answer was deserialized straight into a
    // `Lesson` — must not be able to publish into another bucket by asking.
    let mut forged = lesson("a class of task claiming to be someone else's");
    forged.scope_key = Some("some-other-tenant".to_string());
    let id = a.promote(&forged, &[]).await.expect("promote");

    let stored = a
        .lessons(None)
        .await
        .expect("lessons")
        .into_iter()
        .find(|l| l.id == id)
        .expect("stored");
    assert_eq!(
        stored.scope_key.as_deref(),
        a.scope(),
        "promote must stamp the handle's scope, whatever the argument said"
    );
}

async fn workflow_scores_do_not_bleed_between_tenants(a: &dyn Ledger, b: &dyn Ledger) {
    let id = "wf-shared-id";
    a.score_workflow(id, true).await.expect("score");
    a.score_workflow(id, true).await.expect("score");
    b.score_workflow(id, false).await.expect("score");

    let for_a = a.workflow_score(id).await.expect("score");
    let for_b = b.workflow_score(id).await.expect("score");
    assert_eq!(
        (for_a.applied, for_a.helped),
        (2, 2),
        "tenant a's own record"
    );
    assert_eq!(
        (for_b.applied, for_b.helped),
        (1, 0),
        "tenant b's own record"
    );
}

async fn a_tenant_writing_does_not_move_the_global_score(global: &dyn Ledger, a: &dyn Ledger) {
    let id = "wf-tenant-only";
    a.score_workflow(id, true).await.expect("score");
    let seen = global.workflow_score(id).await.expect("score");
    assert_eq!(
        (seen.applied, seen.helped),
        (0, 0),
        "the global bucket is its own bucket, not a union of every tenant's"
    );
}

/// Run every lineage case. Part of [`run_all`]'s contract for any backend that
/// stores variant links, which is both that ship.
///
/// # Panics
/// On any lineage failure.
pub async fn run_lineage(store: &dyn Ledger) {
    an_unlinked_workflow_is_a_family_of_one(store).await;
    lineage_reads_the_same_from_any_member(store).await;
    linking_the_same_pair_twice_is_a_no_op(store).await;
    a_variant_of_a_variant_stays_in_one_family(store).await;
    a_cycle_is_truncated_rather_than_hung(store).await;
}

async fn an_unlinked_workflow_is_a_family_of_one(store: &dyn Ledger) {
    let family = store.lineage("wf-lonely").await.expect("lineage");
    assert_eq!(family, vec!["wf-lonely".to_string()]);
}

async fn lineage_reads_the_same_from_any_member(store: &dyn Ledger) {
    store
        .link_variant("wf-a", "wf-a-fix-1")
        .await
        .expect("link");
    store
        .link_variant("wf-a", "wf-a-fix-2")
        .await
        .expect("link");

    let from_root = store.lineage("wf-a").await.expect("lineage");
    let from_leaf = store.lineage("wf-a-fix-2").await.expect("lineage");
    assert_eq!(
        from_root, from_leaf,
        "the champion must not depend on which member was asked"
    );
    assert_eq!(
        from_root[0], "wf-a",
        "root first — the fallback relies on it"
    );
    assert_eq!(from_root.len(), 3);
}

async fn linking_the_same_pair_twice_is_a_no_op(store: &dyn Ledger) {
    // A repair converging on an existing variant id will re-link. It must not
    // duplicate the family member.
    store
        .link_variant("wf-b", "wf-b-fix-1")
        .await
        .expect("link");
    store
        .link_variant("wf-b", "wf-b-fix-1")
        .await
        .expect("link");
    assert_eq!(store.lineage("wf-b").await.expect("lineage").len(), 2);
}

async fn a_variant_of_a_variant_stays_in_one_family(store: &dyn Ledger) {
    // `repair` takes whatever ran as the parent, and what ran may itself be a
    // variant. Two generations are still one family, or the grandchild would be
    // compared against nothing.
    store
        .link_variant("wf-c", "wf-c-fix-1")
        .await
        .expect("link");
    store
        .link_variant("wf-c-fix-1", "wf-c-fix-2")
        .await
        .expect("link");

    let family = store.lineage("wf-c-fix-2").await.expect("lineage");
    assert_eq!(family[0], "wf-c");
    assert_eq!(family.len(), 3, "{family:?}");
}

async fn a_cycle_is_truncated_rather_than_hung(store: &dyn Ledger) {
    // Nothing should write this, but the ledger is read on the hot path of
    // every attempt and a hang there stops the whole loop. Bounded walks mean
    // a corrupt link costs a truncated answer instead.
    store.link_variant("wf-y", "wf-x").await.expect("link");
    store.link_variant("wf-x", "wf-y").await.expect("link");
    let family = store.lineage("wf-x").await.expect("lineage");
    assert!(family.len() <= super::MAX_FAMILY, "{family:?}");
}

/// Run every episode-checkpoint case.
///
/// # Panics
/// On any failure. Each is a way a restarted process would lose an episode.
pub async fn run_episodes(store: &dyn Ledger) {
    an_unknown_episode_is_absent_not_an_error(store).await;
    an_episode_round_trips_everything_the_rows_cannot_hold(store).await;
    saving_twice_updates_rather_than_duplicating(store).await;
    running_only_filters_to_the_recovery_list(store).await;
    a_rows_verdict_survives_as_fields_not_as_prose(store).await;
    the_episode_list_is_newest_first_on_every_backend(store).await;
}

async fn the_episode_list_is_newest_first_on_every_backend(store: &dyn Ledger) {
    // `Page` documents newest-first, and paging an unordered list returns
    // opposite ends on different backends — `Page::first(1)` must mean the
    // same episode everywhere.
    for (id, at) in [
        ("ep-ord-old", "2026-02-01T00:00:01Z"),
        ("ep-ord-new", "2026-02-01T00:00:03Z"),
        ("ep-ord-mid", "2026-02-01T00:00:02Z"),
    ] {
        let mut e = episode(id, EpisodeStatus::Running, 1, 0);
        e.updated_at = at.to_string();
        store.save_episode(&e).await.expect("save");
    }
    let ordered: Vec<String> = store
        .episodes(false, super::Page::ALL)
        .await
        .expect("episodes")
        .into_iter()
        .map(|e| e.id)
        .filter(|id| id.starts_with("ep-ord-"))
        .collect();
    assert_eq!(
        ordered,
        ["ep-ord-new", "ep-ord-mid", "ep-ord-old"],
        "newest first, on this backend as on every other"
    );
}

fn episode(id: &str, status: EpisodeStatus, attempt: u32, stalled: u32) -> Episode {
    Episode {
        id: id.to_string(),
        goal: crate::contracts::Goal::new("write the weekly report"),
        scope_key: None,
        status,
        attempt,
        stalled,
        started_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-01T00:00:05Z".to_string(),
    }
}

async fn an_unknown_episode_is_absent_not_an_error(store: &dyn Ledger) {
    assert!(
        store
            .episode("never-started")
            .await
            .expect("read")
            .is_none()
    );
}

async fn an_episode_round_trips_everything_the_rows_cannot_hold(store: &dyn Ledger) {
    let mut want = episode("ep-round", EpisodeStatus::Running, 3, 2);
    want.goal.success_criteria = "cites the actual figures".to_string();
    store.save_episode(&want).await.expect("save");

    let got = store
        .episode("ep-round")
        .await
        .expect("read")
        .expect("saved");
    assert_eq!(
        got.goal.text, want.goal.text,
        "the goal is unrecoverable from rows"
    );
    assert_eq!(got.goal.success_criteria, "cites the actual figures");
    assert_eq!(got.attempt, 3);
    assert_eq!(got.stalled, 2, "the stall count cannot be recomputed");
    assert_eq!(got.status, EpisodeStatus::Running);
}

async fn saving_twice_updates_rather_than_duplicating(store: &dyn Ledger) {
    store
        .save_episode(&episode("ep-twice", EpisodeStatus::Running, 1, 0))
        .await
        .expect("save");
    store
        .save_episode(&episode(
            "ep-twice",
            EpisodeStatus::StoodDown("out of attempts after 12".to_string()),
            12,
            0,
        ))
        .await
        .expect("save");

    let got = store
        .episode("ep-twice")
        .await
        .expect("read")
        .expect("saved");
    assert_eq!(got.attempt, 12);
    match got.status {
        EpisodeStatus::StoodDown(reason) => assert!(reason.contains("out of attempts")),
        other => panic!("expected the second write to win, got {other:?}"),
    }
    let all = store
        .episodes(false, super::Page::ALL)
        .await
        .expect("episodes");
    assert_eq!(
        all.iter().filter(|e| e.id == "ep-twice").count(),
        1,
        "one episode, not two"
    );
}

async fn running_only_filters_to_the_recovery_list(store: &dyn Ledger) {
    store
        .save_episode(&episode("ep-live", EpisodeStatus::Running, 1, 0))
        .await
        .expect("save");
    store
        .save_episode(&episode("ep-won", EpisodeStatus::Satisfied, 2, 0))
        .await
        .expect("save");

    let running = store
        .episodes(true, super::Page::ALL)
        .await
        .expect("episodes");
    assert!(running.iter().any(|e| e.id == "ep-live"));
    assert!(
        !running.iter().any(|e| e.id == "ep-won"),
        "a finished episode is not resumed"
    );
}

async fn a_rows_verdict_survives_as_fields_not_as_prose(store: &dyn Ledger) {
    // `satisfied` used to be recoverable only by matching the outcome string,
    // and `advanced` not at all — so a restart could not recompute the stall.
    let mut won = row("ep-fields", 1, "authored:aaa");
    won.satisfied = true;
    won.advanced = true;
    store.append(&won).await.expect("append");

    let back = &store.rows("ep-fields").await.expect("rows")[0];
    assert!(back.satisfied);
    assert!(back.advanced);
}

/// Run every transcript and paging case.
///
/// # Panics
/// On any failure.
pub async fn run_transcripts(store: &dyn Ledger) {
    an_attempt_with_no_transcript_is_empty_not_an_error(store).await;
    a_transcript_round_trips_in_order(store).await;
    a_looped_node_keeps_every_iteration(store).await;
    saving_a_transcript_twice_replaces_rather_than_appends(store).await;
    a_page_windows_the_episode_list(store).await;
    an_agent_step_keeps_its_harness_transcript(store).await;
}

/// An `agent` step's harness transcript survives the ledger.
///
/// `Ran::steps` is the archival record — "every node activation, at full record
/// fidelity" — so a backend that persists the step but drops what the harness
/// did inside it satisfies the type and loses the point.
async fn an_agent_step_keeps_its_harness_transcript(store: &dyn Ledger) {
    use tinyflows::transcript::TranscriptEntry;

    let entries = vec![
        TranscriptEntry::bounded(1, "agent_thinking", "memoise the chain"),
        TranscriptEntry::bounded(2, "tool_call", "shell: python3 solve.py"),
        TranscriptEntry::bounded(3, "tool_result", "837799"),
    ];
    let mut solve = step("solve", 1);
    solve.transcript = entries.clone();

    store
        .save_steps("ldg_transcript", &[solve, step("check", 2)])
        .await
        .expect("save");

    let back = store.steps("ldg_transcript").await.expect("steps");
    assert_eq!(back.len(), 2);
    assert_eq!(
        back[0].transcript, entries,
        "the agent node's transcript round-trips whole and in order"
    );
    assert!(
        back[1].transcript.is_empty(),
        "a step that recorded none still reads as none, not as the previous step's"
    );
}

fn step(node_id: &str, n: u64) -> crate::execute::StepRecord {
    crate::execute::StepRecord {
        node_id: node_id.to_string(),
        status: crate::execute::StepOutcome::Success,
        output: serde_json::json!({ "i": n }),
        duration_ms: n,
        null_bindings: Vec::new(),
        transcript: Vec::new(),
    }
}

async fn an_attempt_with_no_transcript_is_empty_not_an_error(store: &dyn Ledger) {
    assert!(store.steps("ldg_nothing").await.expect("steps").is_empty());
}

async fn a_transcript_round_trips_in_order(store: &dyn Ledger) {
    let mut errored = step("fetch", 7);
    errored.status = crate::execute::StepOutcome::Error;
    errored.null_bindings = vec![tinyflows::expr::NullResolution {
        location: "args.to".to_string(),
        expression: "=nodes.x.item.email".to_string(),
    }];
    store
        .save_steps("ldg_a", &[step("start", 1), errored])
        .await
        .expect("save");

    let back = store.steps("ldg_a").await.expect("steps");
    assert_eq!(back.len(), 2);
    assert_eq!(back[0].node_id, "start", "execution order is the record");
    assert_eq!(back[1].status, crate::execute::StepOutcome::Error);
    assert_eq!(back[1].duration_ms, 7);
    assert_eq!(
        back[1].null_bindings.len(),
        1,
        "the nested list survives both a JSON column and a native array"
    );
    assert_eq!(back[1].output, serde_json::json!({ "i": 7 }));
}

async fn a_looped_node_keeps_every_iteration(store: &dyn Ledger) {
    // The reason this is a record per step rather than one blob per attempt.
    let steps: Vec<_> = (0..12).map(|n| step("body", n)).collect();
    store.save_steps("ldg_loop", &steps).await.expect("save");

    let back = store.steps("ldg_loop").await.expect("steps");
    assert_eq!(back.len(), 12);
    assert_eq!(
        back.iter().map(|s| s.duration_ms).collect::<Vec<_>>(),
        (0..12).collect::<Vec<_>>(),
        "iterations in order, not deduplicated by node id"
    );
}

async fn saving_a_transcript_twice_replaces_rather_than_appends(store: &dyn Ledger) {
    // A retried write must not double the record.
    store
        .save_steps("ldg_twice", &[step("a", 1), step("b", 2)])
        .await
        .expect("save");
    store
        .save_steps("ldg_twice", &[step("a", 1), step("b", 2)])
        .await
        .expect("save");
    assert_eq!(store.steps("ldg_twice").await.expect("steps").len(), 2);

    // And a SHORTER re-save must not leave the old tail behind — an upsert
    // keyed by sequence replaces only the sequences present, and the stitched
    // result would read as one transcript mixing two attempts.
    store
        .save_steps("ldg_twice", &[step("a", 9)])
        .await
        .expect("save");
    let back = store.steps("ldg_twice").await.expect("steps");
    assert_eq!(back.len(), 1, "{back:?}");
    assert_eq!(
        back[0].duration_ms, 9,
        "and it is the new save, not the old"
    );
}

async fn a_page_windows_the_episode_list(store: &dyn Ledger) {
    for n in 0..5 {
        store
            .save_episode(&episode(
                &format!("ep-page-{n}"),
                EpisodeStatus::Running,
                1,
                0,
            ))
            .await
            .expect("save");
    }
    let all = store
        .episodes(false, super::Page::ALL)
        .await
        .expect("episodes");
    assert!(all.len() >= 5);

    let first_two = store
        .episodes(false, super::Page::first(2))
        .await
        .expect("episodes");
    assert_eq!(first_two.len(), 2);
    assert_eq!(
        first_two.iter().map(|e| &e.id).collect::<Vec<_>>(),
        all[..2].iter().map(|e| &e.id).collect::<Vec<_>>(),
        "the same order, windowed"
    );

    let past_the_end = store
        .episodes(
            false,
            super::Page {
                limit: 10,
                offset: all.len() + 5,
            },
        )
        .await
        .expect("episodes");
    assert!(
        past_the_end.is_empty(),
        "an offset past the end is empty, not a panic"
    );
}
