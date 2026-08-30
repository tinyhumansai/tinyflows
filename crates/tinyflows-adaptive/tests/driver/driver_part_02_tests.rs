fn permissive() -> Arc<dyn tinyflows::store::HostPolicy> {
    #[derive(Debug, Default)]
    struct Permissive;
    impl tinyflows::store::HostPolicy for Permissive {}
    Arc::new(Permissive)
}

#[tokio::test]
async fn the_device_receives_a_variant_only_after_the_goal_run_succeeds() {
    use tinyflows_adaptive::workflows::compat::Layered;
    use tinyflows_adaptive::workflows::conformance::record;
    use tinyflows_adaptive::workflows::memory::MemoryVault;
    use tinyflows_adaptive::workflows::{Snapshot, Vault};

    // The device owns the original; our writable layer starts empty.
    let device = Arc::new(MemoryVault::new());
    device.put(&record("pr-review")).await.expect("put");
    let ours = Arc::new(MemoryVault::new());
    let stacked = Layered::new(
        vec![("device".into(), device.clone() as Arc<dyn Vault>)],
        ours.clone(),
    );

    let snapshot = Snapshot::load(&stacked, permissive()).await.expect("load");
    let store: Arc<dyn WorkflowStore> = Arc::new(snapshot.clone());
    let caps = Capabilities {
        llm: RepairFlow::new(2),
        ..mock_capabilities()
    };
    let ledger = MemoryLedger::new();
    let runner = Local {
        caps: &caps,
        workspace: &Unobserved,
    };

    let finished = engine_over(&ledger, &store, &caps, &runner, &HostFacts::unknown())
        .run("ep-gate", &Goal::new("summarise the open pull requests"))
        .await
        .expect("run");

    assert_eq!(finished.status, EpisodeStatus::Satisfied);
    assert_eq!(
        finished.attempts, 2,
        "the parent failed once and its variant closed the goal"
    );

    // Mid-episode the variant lived only in the snapshot. The gate is the
    // host's one `if`, and it is open:
    assert_eq!(snapshot.pending(), 1);
    snapshot.flush(&stacked).await.expect("flush");

    let landed = ours.load().await.expect("load");
    assert_eq!(landed.len(), 1);
    assert!(
        landed[0].id.starts_with("pr-review-fix-"),
        "{}",
        landed[0].id
    );
    assert_eq!(
        device.load().await.expect("load").len(),
        1,
        "the parent's home holds exactly what it held before"
    );
}

#[tokio::test]
async fn a_failed_goal_run_leaves_no_residue_anywhere_durable() {
    use tinyflows_adaptive::workflows::compat::Layered;
    use tinyflows_adaptive::workflows::conformance::record;
    use tinyflows_adaptive::workflows::memory::MemoryVault;
    use tinyflows_adaptive::workflows::{Snapshot, Vault};

    let device = Arc::new(MemoryVault::new());
    device.put(&record("pr-review")).await.expect("put");
    let ours = Arc::new(MemoryVault::new());
    let stacked = Layered::new(
        vec![("device".into(), device.clone() as Arc<dyn Vault>)],
        ours.clone(),
    );

    let snapshot = Snapshot::load(&stacked, permissive()).await.expect("load");
    let store: Arc<dyn WorkflowStore> = Arc::new(snapshot.clone());
    let caps = Capabilities {
        llm: RepairFlow::new(usize::MAX), // never satisfied; the stall ends it
        ..mock_capabilities()
    };
    let ledger = MemoryLedger::new();
    let runner = Local {
        caps: &caps,
        workspace: &Unobserved,
    };

    let finished = engine_over(&ledger, &store, &caps, &runner, &HostFacts::unknown())
        .run(
            "ep-no-residue",
            &Goal::new("summarise the open pull requests"),
        )
        .await
        .expect("run");

    assert!(matches!(finished.status, EpisodeStatus::StoodDown(_)));
    assert!(
        snapshot.pending() >= 1,
        "repairs were proposed and buffered along the way"
    );

    // The gate stays closed: no flush. The knowledge is not lost with the
    // graphs — the ledger kept the trail, durably, on the server side.
    assert!(ours.load().await.expect("load").is_empty());
    assert_eq!(device.load().await.expect("load").len(), 1);
    assert!(
        !ledger.rows("ep-no-residue").await.expect("rows").is_empty(),
        "the attempts are on the record even though no graph was kept"
    );
}

#[tokio::test]
async fn an_errand_answers_the_goal_without_leaving_a_procedure_behind() {
    // The whole claim of the errand path, end to end: a goal with no procedure
    // in it is answered in one turn, and the shelf is exactly as it was.
    struct Triage {
        tiers: Mutex<Vec<String>>,
    }
    #[async_trait]
    impl LlmProvider for Triage {
        async fn complete(&self, request: Value, _conn: Option<&str>) -> EngineResult<Value> {
            let tier = request["tier"].as_str().unwrap_or_default().to_string();
            self.tiers.lock().expect("lock").push(tier.clone());
            Ok(match tier.as_str() {
                "select" => json!({
                    "workflow_id": null, "errand": true,
                    "why": "one turn of work, no procedure in it"
                }),
                "judge" => json!({
                    "satisfied": true, "blocker": "", "gap": "", "advanced": true
                }),
                // Reached only if the loop wrongly authored or consolidated —
                // both are asserted absent below.
                _ => json!({ "why": "should not be asked", "inputs": {}, "steps": [] }),
            })
        }
    }

    let provider = Arc::new(Triage {
        tiers: Mutex::new(Vec::new()),
    });
    let caps = Capabilities {
        llm: provider.clone(),
        ..mock_capabilities()
    };
    let ledger = MemoryLedger::new();
    let store = store("errand");
    // A workflow on the shelf, so `select` is genuinely asked rather than
    // short-circuited — this test is about the answer, not about the cold-store
    // path, which `select`'s own tests cover.
    let seeded = store.list().expect("list").len();

    let runner = Local {
        caps: &caps,
        workspace: &Unobserved,
    };
    let engine = Loop {
        ledger: &ledger,
        store: &store,
        caps: &caps,
        facts: &HostFacts::unknown(),
        runner: &runner,
        clock: &Frozen,
        budget: Default::default(),
        conn: None,
    };

    let finished = engine
        .run(
            "ep-errand",
            &Goal::new("how much disk is this directory using"),
        )
        .await
        .expect("the episode runs");

    assert_eq!(finished.status, EpisodeStatus::Satisfied);
    assert_eq!(finished.attempts, 1, "one turn, not a retry loop");

    let rows = ledger.rows("ep-errand").await.expect("rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].approach_sig, "errand");
    assert!(
        rows[0].workflow_id.is_none(),
        "an errand scores no procedure, because there is none"
    );

    // The point of the whole path: nothing filed. A one-off on the shelf is
    // worse than useless — it dilutes every later selection with a row that
    // matches once and can never match again.
    assert_eq!(
        store.list().expect("list").len(),
        seeded,
        "an errand must not be kept"
    );
    // Asserted on the *calls*, not on the result. An empty lesson list is also
    // what a consolidator that ran and found nothing returns, so the weaker
    // assertion would pass with the gate removed entirely.
    let tiers = provider.tiers.lock().expect("lock").clone();
    assert!(
        !tiers.iter().any(|t| t == "consolidate"),
        "a plain errand must not pay a consolidation call: {tiers:?}"
    );
    assert!(
        !tiers.iter().any(|t| t == "author"),
        "nor an authoring one: {tiers:?}"
    );
    assert!(finished.lessons.is_empty());
}

#[tokio::test]
async fn a_failed_errand_escalates_to_authoring_instead_of_repeating_itself() {
    // The guard that stops the cheap path becoming a trap. If one turn did not
    // do it, the goal was never an errand — and a model that keeps saying it is
    // must not be able to spend the whole budget on identical single turns.
    struct Insistent {
        seen: Mutex<Vec<String>>,
    }
    #[async_trait]
    impl LlmProvider for Insistent {
        async fn complete(&self, request: Value, _conn: Option<&str>) -> EngineResult<Value> {
            let tier = request["tier"].as_str().unwrap_or_default().to_string();
            self.seen.lock().expect("lock").push(tier.clone());
            Ok(match tier.as_str() {
                // Always insists, every attempt.
                "select" => json!({ "workflow_id": null, "errand": true, "why": "trivial" }),
                "judge" => json!({
                    "satisfied": false, "blocker": "goal_not_met",
                    "gap": "it did not finish", "advanced": false
                }),
                "consolidate" => json!({ "lessons": [], "corroborate": [] }),
                _ => json!({
                    "why": "a real plan",
                    "inputs": {},
                    "steps": [{ "id": "attempt", "run": "echo attempt-done" }],
                }),
            })
        }
    }
    let provider = Arc::new(Insistent {
        seen: Mutex::new(Vec::new()),
    });
    let caps = Capabilities {
        llm: provider.clone(),
        ..mock_capabilities()
    };
    let ledger = MemoryLedger::new();
    let store = store("errand-escalate");
    let runner = Local {
        caps: &caps,
        workspace: &Unobserved,
    };
    let engine = Loop {
        ledger: &ledger,
        store: &store,
        caps: &caps,
        facts: &HostFacts::unknown(),
        runner: &runner,
        clock: &Frozen,
        budget: Default::default(),
        conn: None,
    };

    let goal = Goal::new("do something that only looks trivial");
    engine.attempt("ep-esc", &goal).await.expect("attempt one");
    engine.attempt("ep-esc", &goal).await.expect("attempt two");

    let rows = ledger.rows("ep-esc").await.expect("rows");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].approach_sig, "errand");
    assert!(
        rows[1].approach_sig.starts_with("authored:"),
        "the second attempt must be a real plan, got {}",
        rows[1].approach_sig
    );
}
