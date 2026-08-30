#[tokio::test]
async fn an_authored_graph_that_does_not_validate_is_an_error_not_a_return_value() {
    // Handing it back would turn an authoring mistake into a run-time failure
    // that reads like the work failing. The author retries with the refusal
    // fed back, so the script holds a model that stays wrong for every round.
    let broken = json!({
        "why": "forgot the steps",
        "inputs": {},
    });
    let llm = std::sync::Arc::new(Scripted::new(vec![
        select_declines(),
        broken.clone(),
        broken.clone(),
        broken,
    ]));
    let caps = caps_with(llm);
    let (store, _root) = empty_store("7");
    let ledger = MemoryLedger::new();

    let err = decide(
        &Goal::new("anything"),
        "ep1",
        &store,
        &ledger,
        &HostFacts::unknown(),
        &caps,
        None,
    )
    .await
    .expect_err("an invalid graph must not leave intake");
    assert!(err.to_string().contains("invalid"), "{err}");
}

#[tokio::test]
async fn a_disabled_workflow_is_never_offered() {
    let llm = std::sync::Arc::new(Scripted::new(vec![
        select_declines(),
        authored_reply("written", None),
    ]));
    let caps = caps_with(llm.clone());
    let (store, _root) = empty_store("8");
    let mut off = stored("switched-off", "would have matched", None);
    off.enabled = false;
    store.save(&off).expect("save");
    let ledger = MemoryLedger::new();

    decide(
        &Goal::new("do the thing"),
        "ep1",
        &store,
        &ledger,
        &HostFacts::unknown(),
        &caps,
        None,
    )
    .await
    .expect("decide");

    assert!(
        !llm.prompts()[0].contains("switched-off"),
        "offering a disabled workflow invites a choice that cannot be honoured: {}",
        llm.prompts()[0]
    );
}

#[tokio::test]
async fn a_graph_naming_a_worker_this_host_lacks_is_refused_before_it_runs() {
    // The whole point of collecting host facts. Without this the graph saves
    // cleanly, validates cleanly, and fails at run time — usually overnight,
    // to nobody watching.
    //
    // Three copies: the author feeds refusals back, and this model never
    // learns that the worker does not exist.
    let insistent = json!({
        "why": "needs an agent",
        "inputs": {},
        "steps": [{ "id": "work", "ask": "do the thing", "worker": "desktop" }],
    });
    let llm = std::sync::Arc::new(Scripted::new(vec![
        select_declines(),
        insistent.clone(),
        insistent.clone(),
        insistent,
    ]));
    let caps = caps_with(llm);
    let (store, _root) = empty_store("gated");
    let ledger = MemoryLedger::new();

    let facts = HostFacts {
        workers: vec!["laptop".into(), "ci".into()],
        default_worker: Some("laptop".into()),
        ..HostFacts::unknown()
    };

    let err = decide(
        &Goal::new("do the thing"),
        "ep1",
        &store,
        &ledger,
        &facts,
        &caps,
        None,
    )
    .await
    .expect_err("a worker this host lacks must not reach the engine");

    assert!(
        err.to_string().contains("desktop"),
        "the error names it: {err}"
    );
    assert!(
        err.to_string().contains("laptop"),
        "and offers the alternatives: {err}"
    );
}

#[tokio::test]
async fn the_authoring_prompt_carries_what_the_host_permits() {
    // The facts below say agent work must name a worker, so the reply's ask
    // step names one — the same gate this test exists to see rendered.
    let llm = std::sync::Arc::new(Scripted::new(vec![
        select_declines(),
        json!({
            "why": "fine",
            "inputs": {},
            "steps": [{ "id": "work", "ask": "Do it directly.", "worker": "laptop" }],
        }),
    ]));
    let caps = caps_with(llm.clone());
    let (store, _root) = empty_store("facts-rendered");
    let ledger = MemoryLedger::new();

    let facts = HostFacts {
        workers: vec!["laptop".into()],
        default_worker: None,
        allow_code: Some(false),
        notes: vec!["Only manual triggers fire here.".into()],
        ..HostFacts::unknown()
    };

    decide(
        &Goal::new("anything"),
        "ep1",
        &store,
        &ledger,
        &facts,
        &caps,
        None,
    )
    .await
    .expect("decide");

    let prompt = &authoring_prompt(&llm);
    assert!(prompt.contains("What this host permits"), "{prompt}");
    assert!(prompt.contains("every agent node must name config.agent_ref"));
    assert!(prompt.contains("Only manual triggers fire here."));
}

// ---------------------------------------------------------------------------
// Promotion: a repaired family is one row, and score decides which.
// ---------------------------------------------------------------------------

/// A parent and one variant, both stored and linked, with scores applied.
async fn repaired_family(
    tag: &str,
    parent: (u32, u32),
    variant: (u32, u32),
) -> (FileWorkflowStore, MemoryLedger, std::path::PathBuf) {
    let (store, root) = empty_store(tag);
    store
        .save(&stored("weekly", "writes the weekly report", None))
        .expect("save");
    store
        .save(&stored(
            "weekly-fix-1",
            "writes the weekly report, with the binding corrected",
            None,
        ))
        .expect("save");

    let ledger = MemoryLedger::new();
    ledger
        .link_variant("weekly", "weekly-fix-1")
        .await
        .expect("link");
    for (id, (applied, helped)) in [("weekly", parent), ("weekly-fix-1", variant)] {
        for n in 0..applied {
            ledger.score_workflow(id, n < helped).await.expect("score");
        }
    }
    (store, ledger, root)
}

/// What the selector was actually shown.
async fn offered(store: &FileWorkflowStore, ledger: &MemoryLedger) -> String {
    let llm = std::sync::Arc::new(Scripted::new(vec![
        json!({"workflow_id": "none"}),
        authored_reply("fallback", None),
    ]));
    let caps = caps_with(llm.clone());
    let _ = decide(
        &Goal::new("write the weekly report"),
        "ep-promo",
        store,
        ledger,
        &HostFacts::unknown(),
        &caps,
        None,
    )
    .await;
    llm.prompts().first().cloned().unwrap_or_default()
}

