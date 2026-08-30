#[tokio::test]
async fn the_model_can_still_refuse_a_graph_the_gate_let_through() {
    // Parameterised and reusable-looking, but only meaningful for the one goal
    // it was written for. The gate cannot see that; a reader can.
    let llm = succeeding(parameterised(), false);
    let caps = Capabilities {
        llm: llm.clone(),
        ..mock_capabilities()
    };
    let ledger = MemoryLedger::new();
    let store = store("refused");
    let runner = Local {
        caps: &caps,
        workspace: &Unobserved,
    };

    engine_over(&ledger, &store, &caps, &runner, &HostFacts::unknown())
        .run("ep-refused", &Goal::new("review the PRs on acme/thing"))
        .await
        .expect("run");

    assert!(store.list().expect("list").is_empty());
}

#[tokio::test]
async fn the_next_episode_selects_what_the_last_one_learned() {
    // The whole point, end to end. Episode one finds a cold store and authors;
    // episode two finds the procedure episode one filed.
    let llm = succeeding(parameterised(), true);
    let caps = Capabilities {
        llm: llm.clone(),
        ..mock_capabilities()
    };
    let ledger = MemoryLedger::new();
    let store = store("acquire");
    let runner = Local {
        caps: &caps,
        workspace: &Unobserved,
    };

    engine_over(&ledger, &store, &caps, &runner, &HostFacts::unknown())
        .run("ep-first", &Goal::new("review the PRs on acme/thing"))
        .await
        .expect("first");

    let learned = store.list().expect("list")[0].id.clone();
    llm.seen.lock().expect("lock").clear();

    engine_over(&ledger, &store, &caps, &runner, &HostFacts::unknown())
        .run("ep-second", &Goal::new("review the PRs on other/repo"))
        .await
        .expect("second");

    // The selector was offered it, with the evidence from episode one.
    let offered = llm.seen.lock().expect("lock")[0]["messages"][1]["content"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(offered.contains(&learned), "{offered}");
    assert!(
        offered.contains("run 1×, satisfied 1×"),
        "carrying what it earned: {offered}"
    );
}

// ---------------------------------------------------------------------------
// The two stores are independent: any backend beside any other.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_ledger_and_a_vault_of_different_kinds_drive_the_same_loop() {
    // `Ledger` and `Vault` are separate traits with separate handles, so the
    // host mixes them freely — sqlite ledger beside a Mongo vault, or either
    // beside memory. Nothing in the loop knows which it got.
    use tinyflows_adaptive::ledger::sqlite::SqliteLedger;
    use tinyflows_adaptive::workflows::{Snapshot, Vault, memory::MemoryVault};

    let dir = std::env::temp_dir().join(format!("adaptive-mixed-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    // Durable ledger, ephemeral vault. A deliberately silly pairing, chosen
    // because if this compiles and runs then every sensible one does.
    let ledger = SqliteLedger::open(dir.join("ledger.db")).expect("ledger");
    let vault = MemoryVault::new();

    let llm = succeeding(parameterised(), true);
    let caps = Capabilities {
        llm: llm.clone(),
        ..mock_capabilities()
    };
    let policy: Arc<dyn tinyflows::store::HostPolicy> = {
        #[derive(Debug, Default)]
        struct Permissive;
        impl tinyflows::store::HostPolicy for Permissive {}
        Arc::new(Permissive)
    };
    let snapshot = Snapshot::load(&vault, policy).await.expect("snapshot");
    let store: Arc<dyn WorkflowStore> = Arc::new(snapshot.clone());
    let runner = Local {
        caps: &caps,
        workspace: &Unobserved,
    };

    let finished = engine_over(&ledger, &store, &caps, &runner, &HostFacts::unknown())
        .run("ep-mixed", &Goal::new("review the PRs on acme/thing"))
        .await
        .expect("run");
    assert_eq!(finished.status, EpisodeStatus::Satisfied);

    // The episode landed in sqlite; the learned procedure is waiting in the
    // snapshot for a flush into the vault.
    assert!(ledger.episode("ep-mixed").await.expect("read").is_some());
    assert_eq!(snapshot.pending(), 1, "the procedure it learned");
    snapshot.flush(&vault).await.expect("flush");
    assert_eq!(vault.load().await.expect("load").len(), 1);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_lesson_put_in_front_of_a_planner_is_scored_against_what_happened() {
    // `applied` is the denominator of a lesson's help rate, and nothing was
    // moving it: `score_lesson` had one caller, the corroboration loop, which
    // moves both counters together. So every lesson read 0/0 or n/n, the rate
    // carried no information, and every ordering built on it was inert.
    use tinyflows_adaptive::ledger::{Lesson, LessonKind};

    let llm = succeeding(parameterised(), true);
    let caps = Capabilities {
        llm: llm.clone(),
        ..mock_capabilities()
    };
    let ledger = MemoryLedger::new();
    let id = ledger
        .promote(
            &Lesson {
                id: String::new(),
                kind: LessonKind::Strategy,
                trigger: "a report that must cite figures".into(),
                mechanism: String::new(),
                claim: "read them from the source".into(),
                applied: 0,
                helped: 0,
                scope_key: None,
            },
            &[],
        )
        .await
        .expect("promote");

    let store = store("scored");
    let runner = Local {
        caps: &caps,
        workspace: &Unobserved,
    };
    engine_over(&ledger, &store, &caps, &runner, &HostFacts::unknown())
        .run("ep-scored", &Goal::new("review the PRs on acme/thing"))
        .await
        .expect("run");

    let back = ledger
        .lessons(None)
        .await
        .expect("lessons")
        .into_iter()
        .find(|l| l.id == id)
        .expect("still there");
    assert_eq!(back.applied, 1, "it was shown to the planner");
    assert_eq!(back.helped, 1, "and the episode was satisfied");
}

#[tokio::test]
async fn a_lesson_shown_before_a_failure_moves_only_its_denominator() {
    use tinyflows_adaptive::ledger::{Lesson, LessonKind};

    let llm = authoring(); // its judge always says not-satisfied
    let caps = caps_with(llm);
    let ledger = MemoryLedger::new();
    let id = ledger
        .promote(
            &Lesson {
                id: String::new(),
                kind: LessonKind::Strategy,
                trigger: "a class of task".into(),
                mechanism: String::new(),
                claim: "does not actually help".into(),
                applied: 0,
                helped: 0,
                scope_key: None,
            },
            &[],
        )
        .await
        .expect("promote");

    let store = store("unhelpful");
    let runner = Local {
        caps: &caps,
        workspace: &Unobserved,
    };
    engine_over(&ledger, &store, &caps, &runner, &HostFacts::unknown())
        .run("ep-unhelpful", &Goal::new("write the weekly report"))
        .await
        .expect("run");

    let back = ledger
        .lessons(None)
        .await
        .expect("lessons")
        .into_iter()
        .find(|l| l.id == id)
        .expect("still there");
    assert!(
        back.applied >= 2,
        "shown on every attempt: {}",
        back.applied
    );
    assert_eq!(back.helped, 0, "and it never helped");
}

// ---------------------------------------------------------------------------
// The success gate: a variant exists mid-episode, the device gets it after.
// ---------------------------------------------------------------------------

/// Drives the whole repair story from a script: select the parent, fail it
/// with a node named, propose a fix, select the fix, and — depending on
/// `satisfied_on` — let it win or keep failing until the stall rule ends it.
struct RepairFlow {
    judged: Mutex<usize>,
    satisfied_on: usize,
}

impl RepairFlow {
    fn new(satisfied_on: usize) -> Arc<Self> {
        Arc::new(Self {
            judged: Mutex::new(0),
            satisfied_on,
        })
    }
}

#[async_trait]
impl LlmProvider for RepairFlow {
    async fn complete(&self, request: Value, _conn: Option<&str>) -> EngineResult<Value> {
        let user = request["messages"][1]["content"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        Ok(match request["tier"].as_str().unwrap_or_default() {
            "select" => {
                // Variant ids are content-derived, so the script cannot know
                // them ahead — it reads the listing it was shown, the way a
                // real selector would.
                let ids: Vec<&str> = user
                    .lines()
                    .filter_map(|line| line.trim().strip_prefix("- id: "))
                    .collect();
                let chosen = ids
                    .iter()
                    .find(|id| id.contains("-fix-"))
                    .or_else(|| ids.first());
                json!({ "workflow_id": chosen, "why": "it matches", "inputs": {} })
            }
            "judge" => {
                let mut judged = self.judged.lock().expect("lock");
                *judged += 1;
                if *judged >= self.satisfied_on {
                    json!({ "satisfied": true, "gap": "" })
                } else {
                    json!({
                        "satisfied": false, "blocker": "goal_not_met",
                        "gap": "the summary never landed",
                        "attributed_to": "start", "advanced": false
                    })
                }
            }
            "repair" => json!({
                "ops": [{ "op": "update_node_config", "id": "start",
                          "config": { "note": "fixed" } }],
                "why": "repointed the binding"
            }),
            "consolidate" => json!({ "lessons": [], "corroborate": [] }),
            other => panic!("no `{other}` call belongs in this flow"),
        })
    }
}

