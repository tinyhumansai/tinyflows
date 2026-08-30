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

