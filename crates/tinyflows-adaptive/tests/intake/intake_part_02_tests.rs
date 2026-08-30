#[tokio::test]
async fn a_repaired_family_is_offered_as_one_row_not_two() {
    // Two near-identical graphs whose descriptions differ by a clause is not a
    // choice, it is noise.
    let (store, ledger, _root) = repaired_family("promo-1", (40, 40), (0, 0)).await;
    let shown = offered(&store, &ledger).await;
    let rows = shown.matches("weekly").count();
    assert!(rows > 0, "the family must be offered at all: {shown}");
    assert!(
        !shown.contains("weekly-fix-1"),
        "an unproven variant must not appear beside its proven parent: {shown}"
    );
}

#[tokio::test]
async fn a_fresh_variant_does_not_displace_a_proven_parent() {
    let (store, ledger, _root) = repaired_family("promo-2", (40, 40), (0, 0)).await;
    let shown = offered(&store, &ledger).await;
    assert!(shown.contains("weekly"), "{shown}");
    assert!(!shown.contains("weekly-fix-1"), "{shown}");
}

#[tokio::test]
async fn a_variant_that_has_proven_better_is_the_one_offered() {
    // Promotion on score, not on having been written.
    let (store, ledger, _root) = repaired_family("promo-3", (10, 5), (4, 4)).await;
    let shown = offered(&store, &ledger).await;
    assert!(
        shown.contains("weekly-fix-1"),
        "the better member must take the position: {shown}"
    );
}

#[tokio::test]
async fn a_family_whose_champion_was_already_tried_still_offers_its_variant() {
    // The case that matters most and is easiest to get wrong: this episode just
    // failed with the parent, so the parent is excluded — and the variant
    // exists *because* the parent fell short. Dropping the whole family would
    // hide the one graph written for this exact situation.
    let (store, ledger, _root) = repaired_family("promo-4", (40, 40), (0, 0)).await;
    ledger
        .append(&tinyflows_adaptive::ledger::LedgerRow {
            id: String::new(),
            episode: "ep-promo".into(),
            attempt: 1,
            approach_sig: "selected:weekly".into(),
            approach_desc: "the champion".into(),
            workflow_id: Some("weekly".into()),
            outcome: "fell short".into(),
            cause: String::new(),
            cost_usd: 0.0,
            at: "2026-01-01T00:00:00Z".into(),
            satisfied: false,
            advanced: false,
        })
        .await
        .expect("append");

    let shown = offered(&store, &ledger).await;
    assert!(
        shown.contains("weekly-fix-1"),
        "the variant must survive its champion being excluded: {shown}"
    );
}

// ---------------------------------------------------------------------------
// The retry edge: attempt four must not be attempt two in different words.
// ---------------------------------------------------------------------------

async fn with_history(tag: &str) -> (FileWorkflowStore, MemoryLedger, std::path::PathBuf) {
    let (store, root) = empty_store(tag);
    let ledger = MemoryLedger::new();
    for (attempt, sig, desc, cause) in [
        (
            1u32,
            "authored:aaa",
            "fetched the log and summarised it",
            "no numbers in it",
        ),
        (
            2,
            "authored:bbb",
            "asked an agent to write it from memory",
            "it invented the figures",
        ),
    ] {
        ledger
            .append(&tinyflows_adaptive::ledger::LedgerRow {
                id: String::new(),
                episode: "ep-retry".into(),
                attempt,
                approach_sig: sig.into(),
                approach_desc: desc.into(),
                workflow_id: None,
                outcome: "fell short".into(),
                cause: cause.into(),
                cost_usd: 0.0,
                at: "2026-01-01T00:00:00Z".into(),
                satisfied: false,
                advanced: false,
            })
            .await
            .expect("append");
    }
    (store, ledger, root)
}

#[tokio::test]
async fn the_author_is_shown_what_this_episode_already_tried() {
    // Without this the author writes attempt two's graph again, confidently,
    // because nothing told it otherwise. The exclusion list only guards
    // *selection*; authoring has no structural guard at all.
    let (store, ledger, _root) = with_history("retry-1").await;
    let llm = std::sync::Arc::new(Scripted::new(vec![
        select_declines(),
        authored_reply("third-idea", None),
    ]));
    let caps = caps_with(llm.clone());

    decide(
        &Goal::new("write the weekly report"),
        "ep-retry",
        &store,
        &ledger,
        &HostFacts::unknown(),
        &caps,
        None,
    )
    .await
    .expect("decide");

    let prompt = &authoring_prompt(&llm);
    assert!(prompt.contains("Already tried this episode"), "{prompt}");
    assert!(
        prompt.contains("asked an agent to write it from memory"),
        "{prompt}"
    );
    assert!(prompt.contains("it invented the figures"), "{prompt}");
    assert!(prompt.contains("DIFFERENT plan"), "{prompt}");
}

#[tokio::test]
async fn the_selector_is_shown_the_same_history_in_the_same_words() {
    let (store, ledger, _root) = with_history("retry-2").await;
    store
        .save(&stored("weekly", "writes the weekly report", None))
        .expect("save");

    let llm = std::sync::Arc::new(Scripted::new(vec![json!({
        "workflow_id": "weekly",
        "why": "it does this",
        "inputs": {},
    })]));
    let caps = caps_with(llm.clone());

    decide(
        &Goal::new("write the weekly report"),
        "ep-retry",
        &store,
        &ledger,
        &HostFacts::unknown(),
        &caps,
        None,
    )
    .await
    .expect("decide");

    let prompt = &llm.prompts()[0];
    assert!(prompt.contains("Already tried this episode"), "{prompt}");
    assert!(prompt.contains("no numbers in it"), "{prompt}");
}

#[tokio::test]
async fn lessons_from_other_episodes_reach_the_planner() {
    // consolidate() was writing these and nothing was reading them — a
    // knowledge store that costs money and returns nothing.
    let (store, root) = empty_store("retry-3");
    let _ = root;
    let ledger = MemoryLedger::new();
    ledger
        .promote(
            &tinyflows_adaptive::ledger::Lesson {
                id: String::new(),
                kind: tinyflows_adaptive::ledger::LessonKind::Constraint,
                trigger: "a report that must cite figures".into(),
                mechanism: "the model has no access to the numbers".into(),
                claim: "read them from the source rather than asking an agent".into(),
                applied: 0,
                helped: 0,
                scope_key: None,
            },
            &[],
        )
        .await
        .expect("promote");

    let llm = std::sync::Arc::new(Scripted::new(vec![
        select_declines(),
        authored_reply("informed", None),
    ]));
    let caps = caps_with(llm.clone());

    decide(
        &Goal::new("write the weekly report"),
        "ep-fresh",
        &store,
        &ledger,
        &HostFacts::unknown(),
        &caps,
        None,
    )
    .await
    .expect("decide");

    let prompt = &authoring_prompt(&llm);
    assert!(prompt.contains("Learned from earlier episodes"), "{prompt}");
    assert!(prompt.contains("read them from the source"), "{prompt}");
}

#[tokio::test]
async fn a_first_attempt_is_told_nothing_it_would_have_to_ignore() {
    // An empty history section is noise a model has to read past, and an
    // empty "already tried" heading reads as a claim that something was.
    let (store, _root) = empty_store("retry-4");
    let ledger = MemoryLedger::new();
    let llm = std::sync::Arc::new(Scripted::new(vec![
        select_declines(),
        authored_reply("first", None),
    ]));
    let caps = caps_with(llm.clone());

    decide(
        &Goal::new("write the weekly report"),
        "ep-first",
        &store,
        &ledger,
        &HostFacts::unknown(),
        &caps,
        None,
    )
    .await
    .expect("decide");

    // Every prompt, not just one: an empty heading is noise whichever planner
    // reads it, and the triage call sees the same rendered past the author does.
    for prompt in llm.prompts() {
        assert!(!prompt.contains("Already tried"), "{prompt}");
        assert!(!prompt.contains("Learned from earlier"), "{prompt}");
    }
}

#[tokio::test]
async fn two_authored_attempts_leave_two_distinct_signatures() {
    // The fingerprint end to end: a differently-shaped graph must not fold into
    // the same exclusion-list entry as the one before it.
    let (store, _root) = empty_store("retry-5");
    let ledger = MemoryLedger::new();

    let mut signatures = Vec::new();
    for (n, name) in [(0, "shape-one"), (1, "shape-two")] {
        let llm = std::sync::Arc::new(Scripted::new(vec![
            select_declines(),
            authored_reply(name, if n == 1 { Some("repo") } else { None }),
        ]));
        let attempt = decide(
            &Goal::new("write the weekly report"),
            "ep-sigs",
            &store,
            &ledger,
            &HostFacts::unknown(),
            &caps_with(llm),
            None,
        )
        .await
        .expect("decide");
        signatures.push(attempt.approach.signature());
    }

    assert_ne!(signatures[0], signatures[1], "{signatures:?}");
    assert!(signatures[0].starts_with("authored:"), "{signatures:?}");
}
