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
