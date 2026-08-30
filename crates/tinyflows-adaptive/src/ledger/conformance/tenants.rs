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
    a_global_lessons_evidence_is_visible_to_every_tenant(global, a, b).await;
    promote_stamps_the_handle_not_the_argument(a).await;
    workflow_scores_do_not_bleed_between_tenants(a, b).await;
    a_tenant_writing_does_not_move_the_global_score(global, a).await;
    an_episode_id_alone_does_not_reach_another_tenants_attempts(a, b).await;
    identical_episode_ids_are_stored_once_per_tenant(a, b).await;
    naming_another_tenants_lesson_id_does_not_move_its_score(global, a, b).await;
}

async fn a_global_lessons_evidence_is_visible_to_every_tenant(
    global: &dyn Ledger,
    a: &dyn Ledger,
    b: &dyn Ledger,
) {
    let row_id = global
        .append(&row("ep-global-evidence", 1, "authored:global"))
        .await
        .expect("append global evidence");
    let lesson_id = global
        .promote(&lesson("a globally useful class of task"), &[row_id])
        .await
        .expect("promote global lesson");
    for tenant in [a, b] {
        let evidence = tenant.evidence(&lesson_id).await.expect("global evidence");
        assert_eq!(evidence.len(), 1, "tenant {:?}", tenant.scope());
        assert_eq!(evidence[0].episode, "ep-global-evidence");
    }
}

async fn identical_episode_ids_are_stored_once_per_tenant(a: &dyn Ledger, b: &dyn Ledger) {
    let mut for_a = episode("ep-shared-id", EpisodeStatus::Running, 1, 0);
    for_a.goal.text = "tenant a goal".to_string();
    let mut for_b = episode("ep-shared-id", EpisodeStatus::Running, 2, 0);
    for_b.goal.text = "tenant b goal".to_string();

    a.save_episode(&for_a).await.expect("save tenant a");
    b.save_episode(&for_b).await.expect("save tenant b");

    let got_a = a
        .episode("ep-shared-id")
        .await
        .expect("read tenant a")
        .expect("tenant a episode");
    let got_b = b
        .episode("ep-shared-id")
        .await
        .expect("read tenant b")
        .expect("tenant b episode");
    assert_eq!(got_a.goal.text, "tenant a goal");
    assert_eq!(got_a.attempt, 1);
    assert_eq!(got_b.goal.text, "tenant b goal");
    assert_eq!(got_b.attempt, 2);
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
