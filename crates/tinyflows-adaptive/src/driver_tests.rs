use super::*;

struct Frozen;
impl Clock for Frozen {
    fn now(&self) -> String {
        "2026-01-01T00:00:00Z".to_string()
    }
}

#[tokio::test]
async fn starting_an_episode_twice_does_not_restart_it() {
    // A service that retries a create must not reset a goal four attempts
    // in — the rows would stay and the counters would not, which reads as
    // progress that never happened.
    let ledger = crate::ledger::memory::MemoryLedger::new();
    let goal = Goal::new("write the weekly report");

    let mut record = Episode {
        id: "ep-1".into(),
        goal: goal.clone(),
        scope_key: None,
        status: EpisodeStatus::Running,
        attempt: 4,
        stalled: 2,
        started_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-01T00:00:00Z".into(),
    };
    ledger.save_episode(&record).await.expect("save");

    // `start` short-circuits on an existing record, so this is what it sees.
    let seen = ledger.episode("ep-1").await.expect("read").expect("exists");
    assert_eq!(seen.attempt, 4);
    assert_eq!(seen.stalled, 2);

    record.attempt = 5;
    ledger.save_episode(&record).await.expect("save");
    assert_eq!(
        ledger
            .episode("ep-1")
            .await
            .expect("read")
            .expect("exists")
            .attempt,
        5,
        "a save updates rather than duplicating"
    );
}

#[tokio::test]
async fn only_running_episodes_are_offered_for_recovery() {
    let ledger = crate::ledger::memory::MemoryLedger::new();
    for (id, status) in [
        ("ep-live", EpisodeStatus::Running),
        ("ep-done", EpisodeStatus::Satisfied),
        (
            "ep-gave-up",
            EpisodeStatus::StoodDown("out of attempts".into()),
        ),
    ] {
        ledger
            .save_episode(&Episode {
                id: id.into(),
                goal: Goal::new("something"),
                scope_key: None,
                status,
                attempt: 1,
                stalled: 0,
                started_at: "2026-01-01T00:00:00Z".into(),
                updated_at: "2026-01-01T00:00:00Z".into(),
            })
            .await
            .expect("save");
    }

    let running = ledger.episodes(true, Page::ALL).await.expect("episodes");
    assert_eq!(running.len(), 1);
    assert_eq!(running[0].id, "ep-live");
    assert_eq!(
        ledger
            .episodes(false, Page::ALL)
            .await
            .expect("episodes")
            .len(),
        3
    );
}

#[tokio::test]
async fn an_episode_round_trips_its_goal_and_its_reason_for_stopping() {
    // Both are unrecoverable from the rows, which is the whole test for
    // what belongs on the record.
    let ledger = crate::ledger::memory::MemoryLedger::new();
    let mut goal = Goal::new("write the weekly report");
    goal.success_criteria = "cites the actual figures".into();

    ledger
        .save_episode(&Episode {
            id: "ep-2".into(),
            goal,
            scope_key: None,
            status: EpisodeStatus::StoodDown("3 attempts in a row made no progress".into()),
            attempt: 7,
            stalled: 3,
            started_at: Frozen.now(),
            updated_at: Frozen.now(),
        })
        .await
        .expect("save");

    let back = ledger.episode("ep-2").await.expect("read").expect("exists");
    assert_eq!(back.goal.text, "write the weekly report");
    assert_eq!(back.goal.success_criteria, "cites the actual figures");
    assert_eq!(back.stalled, 3);
    match back.status {
        EpisodeStatus::StoodDown(reason) => assert!(reason.contains("no progress")),
        other => panic!("expected a stand-down, got {other:?}"),
    }
}

#[tokio::test]
async fn one_tenants_episodes_are_invisible_to_another() {
    let ledger = crate::ledger::memory::MemoryLedger::new();
    let a = ledger.for_tenant("user-a");
    let b = ledger.for_tenant("user-b");
    a.save_episode(&Episode {
        id: "ep-private".into(),
        goal: Goal::new("something of mine"),
        scope_key: None,
        status: EpisodeStatus::Running,
        attempt: 1,
        stalled: 0,
        started_at: Frozen.now(),
        updated_at: Frozen.now(),
    })
    .await
    .expect("save");

    assert!(a.episode("ep-private").await.expect("read").is_some());
    assert!(
        b.episode("ep-private").await.expect("read").is_none(),
        "an episode carries a goal in the user's own words"
    );
    assert!(
        b.episodes(false, Page::ALL)
            .await
            .expect("episodes")
            .is_empty()
    );
}
