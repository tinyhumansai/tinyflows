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

include!("driver/driver_part_01_tests.rs");
include!("driver/driver_part_02_tests.rs");
