//! Two arms over one task family, differing only in whether learning is on.
//!
//! Run it:
//!
//! ```text
//! cargo run -p tinyflows-adaptive --example eval
//! ```
//!
//! The claim is not "it solves things" — a retry loop does that. The claim is
//! that attempts-to-success **declines across a family**, so the eval needs
//! repetition within a distribution or it measures nothing. Same tasks, same
//! order, both arms; one arm keeps what it learns between episodes and the
//! other starts each episode empty.
//!
//! Everything here is the real crate: a real [`Loop`], a real ledger, real
//! intake and closing. Two things are stand-ins, and both have to be —
//!
//! * **the model**, scripted so the arms differ by the variable under test and
//!   not by sampling noise. A real eval points this at a provider and accepts
//!   that a null result may be the model rather than the loop;
//! * **the runner**, which "solves" a task in fewer attempts when it has been
//!   told how. That is the thing being simulated: a harness that benefits from
//!   being handed a lesson. Point it at a real one and the shape is unchanged.
//!
//! What it demonstrates, in order:
//!
//! 1. the two arms — the same [`Loop`] wiring, with [`Forgetful`] and a fresh
//!    store on the control side;
//! 2. an [`Episode`] read off what the loop already recorded, rather than off
//!    a parallel bookkeeping nobody uses;
//! 3. the report, and the comparison that refuses to call a bend a win unless
//!    the arm also converges.
//!
//! **`--live` is not implemented here on purpose.** A real family has to be
//! chosen for shared technique and checkable answers, and belongs to whoever
//! runs the eval; this crate is host-agnostic and a task list baked into it
//! would be an opinion about somebody else's domain.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tinyflows::caps::mock::mock_capabilities;
use tinyflows::caps::{Capabilities, LlmProvider};
use tinyflows::error::Result as EngineResult;
use tinyflows::store::{FileWorkflowStore, WorkflowStore};
use tinyflows_adaptive::contracts::{Budget, Goal};
use tinyflows_adaptive::driver::{Clock, Loop};
use tinyflows_adaptive::evals::{Episode, Experiment, Forgetful, LEARNING_OFF, LEARNING_ON};
use tinyflows_adaptive::execute::{Ran, RunReport, Runner, StepOutcome, StepRecord};
use tinyflows_adaptive::host::HostFacts;
use tinyflows_adaptive::intake::Attempt;
use tinyflows_adaptive::ledger::{Ledger, memory::MemoryLedger};

/// The family. One technique, several surfaces — which is what makes a lesson
/// about *approach* rather than about one task, and therefore what makes the
/// second episode able to benefit from the first.
const FAMILY: &str = "cache-and-build-upward";

const TASKS: [&str; 5] = [
    "the longest Collatz chain under a million",
    "the number of routes through a 20x20 grid",
    "the ways to make 200 pence from the usual coins",
    "the ways 100 can be written as a sum of at least two positives",
    "the first value expressible as a sum of primes in over five thousand ways",
];

struct Frozen;
impl Clock for Frozen {
    fn now(&self) -> String {
        "2026-01-01T00:00:00Z".to_string()
    }
}

/// A scripted model, and the one place the simulation lives.
///
/// The judge accepts once the runner has done the work, the author writes a
/// one-step plan, and consolidation promotes the family's technique as a
/// lesson **only when it has actually been observed** — a lesson invented
/// before the evidence would make the control arm lose to a fabrication.
struct Scripted;

#[async_trait]
impl LlmProvider for Scripted {
    async fn complete(&self, request: Value, _conn: Option<&str>) -> EngineResult<Value> {
        let prompt = request["messages"]
            .as_array()
            .and_then(|m| m.last())
            .and_then(|m| m["content"].as_str())
            .unwrap_or_default();
        // The lesson is only useful because the planner is shown it — this is
        // the whole mechanism the eval is measuring, so it is worth seeing.
        let knows_the_technique = prompt.contains("cache the sub-results");
        Ok(match request["tier"].as_str().unwrap_or_default() {
            "select" => json!({ "workflow_id": null, "why": "nothing stored fits yet" }),
            "author" => json!({
                "why": "compute it, then record the answer",
                "declared": [],
                "inputs": {},
                "steps": [{
                    "id": "solve",
                    "run": if knows_the_technique {
                        "echo cached-and-built-upward"
                    } else {
                        "echo enumerated"
                    },
                }],
            }),
            "judge" => json!({
                "satisfied": prompt.contains("cached-and-built-upward"),
                "blocker": "goal_not_met",
                "gap": "the obvious enumeration does not finish",
                "advanced": true,
            }),
            // `evidence` — the row numbers the prompt showed — and not
            // `cites`. `consolidate` refuses a lesson with nothing behind it,
            // so the wrong key means the lesson is dropped in silence and the
            // treatment arm learns nothing. Both arms then read flat, which
            // looks like a null result rather than a typo.
            "consolidate" => json!({
                "lessons": [{
                    "kind": "strategy",
                    "trigger": "a CPU-bound scan whose obvious enumeration blows up",
                    "mechanism": "the sub-results repeat, so recomputing them dominates",
                    "claim": "cache the sub-results and build upward",
                    "evidence": [0],
                }],
                "corroborate": [],
            }),
            _ => json!({}),
        })
    }
}

/// A runner that succeeds when the plan reflects the technique, and burns an
/// attempt when it does not.
///
/// This is the simulated half: a real harness benefits from being told how to
/// approach a problem, and an eval with a runner that ignores its brief would
/// measure nothing whatever the loop did.
struct Simulated;

#[async_trait]
impl Runner for Simulated {
    async fn run(&self, attempt: &Attempt) -> Ran {
        let script = attempt
            .graph
            .nodes
            .iter()
            .find_map(|node| node.config.get("source").and_then(Value::as_str))
            .unwrap_or_default()
            .to_string();
        let worked = script.contains("cached-and-built-upward");
        RunReport {
            steps: vec![StepRecord {
                node_id: "solve".to_string(),
                status: StepOutcome::Success,
                output: json!({ "json": { "exit_code": 0, "stdout": script } }),
                duration_ms: 1,
                null_bindings: Vec::new(),
                transcript: Vec::new(),
            }],
            changed: if worked {
                "wrote the answer".to_string()
            } else {
                "the run did not finish in time".to_string()
            },
            cost_usd: 0.25,
            ..RunReport::default()
        }
        .into_ran(&attempt.graph)
    }
}

/// A workflow store under a scratch directory of its own.
///
/// Cleared on the way in rather than on the way out, so a run that panicked
/// half way through does not quietly seed the next one — the treatment arm
/// starting with a workflow it did not earn would fake the entire result.
fn store(tag: &str) -> Arc<dyn WorkflowStore> {
    let root = std::env::temp_dir().join(format!("adaptive-eval-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("workflows")).expect("scratch dir");
    Arc::new(FileWorkflowStore::new(
        vec![root.join("workflows")],
        root.join("runs"),
    ))
}

/// Run one arm over the whole family, in order.
///
/// The treatment arm keeps one ledger and one store across every episode. The
/// control arm gets [`Forgetful`] over the same ledger — so it still plans
/// within an episode — and a store per episode, because a kept workflow is
/// learning too and would otherwise leak across the boundary the arms are
/// supposed to differ on.
async fn arm(label: &str, experiment: &mut Experiment) {
    let caps = Capabilities {
        llm: Arc::new(Scripted),
        ..mock_capabilities()
    };
    let learning = label == LEARNING_ON;
    let shared = MemoryLedger::new();
    let shared_store = store(&format!("{label}-kept"));

    for (index, task) in TASKS.iter().enumerate() {
        let workflows = if learning {
            shared_store.clone()
        } else {
            // A kept workflow is learning too, so the control arm gets a store
            // per episode. Without this the arms would differ in less than
            // they appear to, and the result would be worth nothing.
            store(&format!("{label}-{index}"))
        };
        let ledger: Box<dyn Ledger> = if learning {
            Box::new(shared.clone())
        } else {
            Box::new(Forgetful::new(shared.clone()))
        };

        let engine = Loop {
            ledger: ledger.as_ref(),
            store: &workflows,
            caps: &caps,
            facts: &HostFacts::unknown(),
            runner: &Simulated,
            clock: &Frozen,
            budget: Budget {
                attempts: 4,
                ..Budget::default()
            },
            conn: None,
        };

        let episode = format!("{label}-{index}");
        let finished = engine
            .run(&episode, &Goal::new(format!("Compute {task}.")))
            .await
            .expect("the episode runs");

        // Read off the ledger rather than off the runner: the ledger is what
        // the loop is claimed to learn from, so measuring anything else would
        // measure a bookkeeping nobody uses.
        let rows = shared.rows(&episode).await.expect("rows");
        let measured = Episode::of(&format!("t{index}"), FAMILY, &finished, &rows);
        println!(
            "  {}. {:<62} {:?}  attempts={}  ${:.2}",
            index + 1,
            task,
            measured.outcome(),
            measured.attempts,
            measured.cost_usd
        );
        if std::env::var("EVAL_DEBUG").is_ok() {
            let known = shared.lessons(None).await.expect("lessons");
            eprintln!(
                "     [debug] lessons now: {} {:?}",
                known.len(),
                known.iter().map(|l| l.claim.as_str()).collect::<Vec<_>>()
            );
        }
        experiment.record(label, measured);
    }
}

#[tokio::main]
async fn main() {
    let mut experiment = Experiment::new(FAMILY);
    for label in [LEARNING_ON, LEARNING_OFF] {
        println!("\n{}\n{label}\n{}", "=".repeat(76), "=".repeat(76));
        arm(label, &mut experiment).await;
    }

    println!("\n{}\nREPORT\n{}", "=".repeat(76), "=".repeat(76));
    println!(
        "{}",
        serde_json::to_string_pretty(&experiment.report()).expect("report")
    );
    println!("\nlearning_helps: {}", experiment.learning_helps());
}
