//! One instance, many goal runs, and an episode that outlives the process.
//!
//! These test the claim the `driver` module is built on, because it is the one
//! that is expensive to be wrong about: a `Loop` is per **tenant** and a goal
//! run is an **episode id**, so the same instance drives many episodes at once
//! and any instance can pick up an episode any other one started.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value, json};
use tinyflows::caps::mock::mock_capabilities;
use tinyflows::caps::{Capabilities, LlmProvider};
use tinyflows::error::Result as EngineResult;
use tinyflows::store::{FileWorkflowStore, WorkflowStore};
use tinyflows_adaptive::contracts::Goal;
use tinyflows_adaptive::driver::{Clock, Loop};
use tinyflows_adaptive::execute::{Local, Unobserved};
use tinyflows_adaptive::host::HostFacts;
use tinyflows_adaptive::ledger::{EpisodeStatus, Ledger, Page, memory::MemoryLedger};

struct Frozen;
impl Clock for Frozen {
    fn now(&self) -> String {
        "2026-01-01T00:00:00Z".to_string()
    }
}

/// Answers every authoring call the same way, and keeps every request so the
/// tier can be read back off the wire.
struct Always {
    reply: Value,
    seen: Mutex<Vec<Value>>,
}

impl Always {
    fn new(reply: Value) -> Arc<Self> {
        Arc::new(Self {
            reply,
            seen: Mutex::new(Vec::new()),
        })
    }
    fn tiers(&self) -> Vec<String> {
        self.seen
            .lock()
            .expect("lock")
            .iter()
            .map(|r| r["tier"].as_str().unwrap_or("(absent)").to_string())
            .collect()
    }
}

#[async_trait]
impl LlmProvider for Always {
    async fn complete(&self, request: Value, _conn: Option<&str>) -> EngineResult<Value> {
        self.seen.lock().expect("lock").push(request.clone());
        // The tier says which job is asking, so one double can answer them all.
        Ok(match request["tier"].as_str().unwrap_or_default() {
            "judge" => json!({
                "satisfied": false, "blocker": "goal_not_met",
                "gap": "the report has no numbers in it", "advanced": false
            }),
            "consolidate" => json!({ "lessons": [], "corroborate": [] }),
            "select" => json!({ "workflow_id": null, "why": "nothing fits" }),
            _ => self.reply.clone(),
        })
    }
}

fn caps_with(llm: Arc<Always>) -> Capabilities {
    Capabilities {
        llm,
        ..mock_capabilities()
    }
}

fn store(tag: &str) -> Arc<dyn WorkflowStore> {
    let root = std::env::temp_dir().join(format!("adaptive-driver-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("workflows")).expect("temp dir");
    Arc::new(FileWorkflowStore::new(
        vec![root.join("workflows")],
        root.join("runs"),
    ))
}

fn authoring() -> Arc<Always> {
    Always::new(json!({
        "why": "nothing stored fits",
        "inputs": {},
        "steps": [{ "id": "attempt", "run": "echo attempt-done" }],
    }))
}

#[tokio::test]
async fn one_instance_drives_two_goal_runs_with_independent_counters() {
    // The claim the split rests on: the instance holds no per-episode state, so
    // two episodes interleaved through it cannot contaminate each other.
    let llm = authoring();
    let caps = caps_with(llm);
    let ledger = MemoryLedger::new();
    let store = store("two");
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

    let goal = Goal::new("write the weekly report");
    engine.attempt("ep-a", &goal).await.expect("a1");
    engine.attempt("ep-b", &goal).await.expect("b1");
    engine.attempt("ep-a", &goal).await.expect("a2");

    let a = ledger.episode("ep-a").await.expect("read").expect("exists");
    let b = ledger.episode("ep-b").await.expect("read").expect("exists");
    assert_eq!(a.attempt, 2);
    assert_eq!(b.attempt, 1, "b is untouched by a's two passes");
    assert_eq!(a.stalled, 2, "neither of a's attempts advanced");
    assert_eq!(b.stalled, 1);

    assert_eq!(ledger.rows("ep-a").await.expect("rows").len(), 2);
    assert_eq!(ledger.rows("ep-b").await.expect("rows").len(), 1);
}

#[tokio::test]
async fn a_second_instance_picks_up_an_episode_the_first_one_started() {
    // Kill the process mid-episode. Everything the loop needs is in the ledger,
    // so a fresh instance continues the numbering rather than starting over
    // with a trail that says it has already tried twice.
    let ledger = MemoryLedger::new();
    let store = store("resume");
    let goal = Goal::new("write the weekly report");

    {
        let caps = caps_with(authoring());
        let runner = Local {
            caps: &caps,
            workspace: &Unobserved,
        };
        let first = Loop {
            ledger: &ledger,
            store: &store,
            caps: &caps,
            facts: &HostFacts::unknown(),
            runner: &runner,
            clock: &Frozen,
            budget: Default::default(),
            conn: None,
        };
        first.attempt("ep-resume", &goal).await.expect("1");
        first.attempt("ep-resume", &goal).await.expect("2");
    } // the instance goes away, as a deploy would take it

    let unfinished = ledger.episodes(true, Page::ALL).await.expect("episodes");
    assert_eq!(unfinished.len(), 1, "the recovery list a boot reads");
    let recovered = &unfinished[0];
    assert_eq!(recovered.id, "ep-resume");
    assert_eq!(recovered.goal.text, "write the weekly report");
    assert_eq!(recovered.stalled, 2);

    let caps = caps_with(authoring());
    let runner = Local {
        caps: &caps,
        workspace: &Unobserved,
    };
    let second = Loop {
        ledger: &ledger,
        store: &store,
        caps: &caps,
        facts: &HostFacts::unknown(),
        runner: &runner,
        clock: &Frozen,
        budget: Default::default(),
        conn: None,
    };
    let closed = second
        .attempt(&recovered.id, &recovered.goal)
        .await
        .expect("3");

    assert_eq!(
        ledger
            .episode("ep-resume")
            .await
            .expect("read")
            .expect("exists")
            .attempt,
        3,
        "it continued rather than restarting at one"
    );
    assert_eq!(
        closed.stalled, 3,
        "the stall count survived the process that was counting it"
    );
}

#[tokio::test]
async fn every_inference_request_says_which_job_is_asking() {
    // Without this a host cannot route judging and selecting to different
    // models, which is the whole point of the tier.
    let llm = authoring();
    let caps = caps_with(llm.clone());
    let ledger = MemoryLedger::new();
    let store = store("tiers");
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

    engine
        .attempt("ep-tiers", &Goal::new("write the weekly report"))
        .await
        .expect("attempt");

    let tiers = llm.tiers();
    assert!(!tiers.iter().any(|t| t == "(absent)"), "{tiers:?}");
    assert!(tiers.contains(&"author".to_string()), "{tiers:?}");
    assert!(tiers.contains(&"judge".to_string()), "{tiers:?}");
}

#[tokio::test]
async fn a_run_drives_to_a_stand_down_and_consolidates_once() {
    // The judge never says satisfied and nothing advances, so the stall rule
    // ends it. `run` must stop on its own rather than needing a bound of its
    // own alongside the one `close` already applies.
    let llm = authoring();
    let caps = caps_with(llm.clone());
    let ledger = MemoryLedger::new();
    let store = store("drive");
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
        .run("ep-drive", &Goal::new("write the weekly report"))
        .await
        .expect("run");

    match &finished.status {
        EpisodeStatus::StoodDown(reason) => assert!(reason.contains("no progress"), "{reason}"),
        other => panic!("expected a stand-down, got {other:?}"),
    }
    assert!(finished.attempts >= 2, "{finished:?}");
    assert!(finished.lessons.is_empty(), "nothing generalised");

    // Consolidation is per episode, not per attempt.
    assert_eq!(
        llm.tiers().iter().filter(|t| *t == "consolidate").count(),
        1
    );

    let record = ledger
        .episode("ep-drive")
        .await
        .expect("read")
        .expect("exists");
    assert!(matches!(record.status, EpisodeStatus::StoodDown(_)));
    assert_ne!(
        record.status,
        EpisodeStatus::Running,
        "a finished episode must leave the recovery list"
    );
}

// ---------------------------------------------------------------------------
// The loop acquires a skill: authored, worked, kept, then selected.
// ---------------------------------------------------------------------------

/// A graph parameterised by a declared input, which is what the authoring
/// prompt asks for and what makes a procedure worth keeping.
fn parameterised() -> Value {
    json!({
        "why": "review",
        "declared": [{ "name": "repo", "description": "the repository", "required": true }],
        "inputs": { "repo": "acme/thing" },
        "steps": [{
            "id": "review",
            "ask": "Review the open pull requests and summarise them directly."
        }],
    })
}

/// Authors `graph`, judges every run satisfied, and answers the naming call.
struct Succeeds {
    authored: Value,
    reusable: bool,
    seen: Mutex<Vec<Value>>,
}

#[async_trait]
impl LlmProvider for Succeeds {
    async fn complete(&self, request: Value, _conn: Option<&str>) -> EngineResult<Value> {
        self.seen.lock().expect("lock").push(request.clone());
        Ok(match request["tier"].as_str().unwrap_or_default() {
            "judge" => json!({ "satisfied": true, "gap": "" }),
            "consolidate" => json!({ "lessons": [], "corroborate": [] }),
            "select" => json!({ "workflow_id": null, "why": "nothing fits yet" }),
            "generalise" => json!({
                "name": "Review a repository's pull requests",
                "description": "Reviews the open pull requests on a repository. Takes the repository as an input.",
                "reusable": self.reusable,
            }),
            _ => self.authored.clone(),
        })
    }
}

fn succeeding(authored: Value, reusable: bool) -> Arc<Succeeds> {
    Arc::new(Succeeds {
        authored,
        reusable,
        seen: Mutex::new(Vec::new()),
    })
}

fn engine_over<'a>(
    ledger: &'a dyn Ledger,
    store: &'a Arc<dyn WorkflowStore>,
    caps: &'a Capabilities,
    runner: &'a Local<'a>,
    facts: &'a HostFacts,
) -> Loop<'a> {
    Loop {
        ledger,
        store,
        caps,
        facts,
        runner,
        clock: &Frozen,
        budget: Default::default(),
        conn: None,
    }
}

#[tokio::test]
async fn a_graph_that_was_authored_and_worked_becomes_a_stored_procedure() {
    // The headline claim: "selects a stored workflow or authors one" is only
    // half true if authoring never becomes stored, because then the catalogue
    // holds exactly what a person put there and the loop never acquires a skill.
    let llm = succeeding(parameterised(), true);
    let caps = Capabilities {
        llm: llm.clone(),
        ..mock_capabilities()
    };
    let ledger = MemoryLedger::new();
    let store = store("keep");
    let runner = Local {
        caps: &caps,
        workspace: &Unobserved,
    };

    assert!(store.list().expect("list").is_empty(), "a cold store");

    let finished = engine_over(&ledger, &store, &caps, &runner, &HostFacts::unknown())
        .run("ep-learn", &Goal::new("review the PRs on acme/thing"))
        .await
        .expect("run");
    assert_eq!(finished.status, EpisodeStatus::Satisfied);

    let listed = store.list().expect("list");
    assert_eq!(listed.len(), 1, "the procedure was filed: {listed:?}");
    assert!(listed[0].id.starts_with("learned-"), "{}", listed[0].id);
    assert!(
        listed[0].description.contains("a repository"),
        "described as a class, not as the goal: {}",
        listed[0].description
    );

    // Scored from the run that earned it — entering at 0/0 would be
    // indistinguishable from a procedure nobody has ever run.
    let score = ledger.workflow_score(&listed[0].id).await.expect("score");
    assert_eq!((score.applied, score.helped), (1, 1));
}

#[tokio::test]
async fn a_graph_that_pasted_its_inputs_is_not_kept() {
    // Same run, same success — but the goal's specifics are welded into a
    // step, so it matches one task and never another. No model is asked.
    //
    // The paste sits in a `run` script, not an ask: the intake gate refuses
    // ask-pastes outright now, and this test is about the layer BEHIND it —
    // keep's own refusal, which still guards every path intake cannot see.
    let mut baked = parameterised();
    baked["steps"] = json!([
        { "id": "review", "run": "gh pr list -R acme/thing" },
        { "id": "report", "ask": "Summarise the review output.", "reads": ["review"] }
    ]);

    let llm = succeeding(baked, true);
    let caps = Capabilities {
        llm: llm.clone(),
        ..mock_capabilities()
    };
    let ledger = MemoryLedger::new();
    let store = store("baked");
    let runner = Local {
        caps: &caps,
        workspace: &Unobserved,
    };

    let finished = engine_over(&ledger, &store, &caps, &runner, &HostFacts::unknown())
        .run("ep-baked", &Goal::new("review the PRs on acme/thing"))
        .await
        .expect("run");

    assert_eq!(finished.status, EpisodeStatus::Satisfied, "it still worked");
    assert!(
        store.list().expect("list").is_empty(),
        "but it is a one-off"
    );

    let tiers: Vec<String> = llm
        .seen
        .lock()
        .expect("lock")
        .iter()
        .map(|r| r["tier"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(
        !tiers.iter().any(|t| t == "generalise"),
        "the mechanical gate settled it without paying for an opinion: {tiers:?}"
    );
}

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
