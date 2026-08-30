//! The whole chain, driven by the real loop: a run breaks, the repair is safe,
//! and the next attempt picks the run back up instead of redoing it.
//!
//! Every other test of this feature covers one joint. The gate's unit tests ask
//! whether an edit is safe; the engine's e2e asks whether a continue re-enters
//! the right node. Neither can catch the chain being wired up wrong — a gate
//! that says yes to a `ResumePoint` nobody threads through, a runner handed one
//! for a workflow the chooser did not pick. This drives `Loop::run` end to end
//! against a real engine, a real checkpointer and a real store, and asserts on
//! an **invocation counter**: the effectful first step must happen exactly once
//! across both attempts.
//!
//! Only two things are doubles, and both have to be: the model (scripted, so
//! the repair is a known edit rather than a guess) and the tool (counting, so
//! "did the prefix run again" is answerable at all).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value, json};
use tinyflows::caps::mock::mock_capabilities;
use tinyflows::caps::{Capabilities, LlmProvider, ToolInvoker};
use tinyflows::compiler::compile;
use tinyflows::engine::{
    InMemoryCheckpointer, RunInput, failure_boundary, retry_with_checkpointer,
    run_with_checkpointer,
};
use tinyflows::error::{EngineError, Result as EngineResult};
use tinyflows::model::{Edge, Node, NodeKind, WorkflowGraph};
use tinyflows::store::{FileWorkflowStore, WorkflowRecord, WorkflowStore};
use tinyflows_adaptive::contracts::{Goal, ResumePoint};
use tinyflows_adaptive::driver::{Clock, Loop};
use tinyflows_adaptive::execute::{Ran, RunReport, Runner, StepOutcome, StepRecord};
use tinyflows_adaptive::host::HostFacts;
use tinyflows_adaptive::intake::Attempt;
use tinyflows_adaptive::ledger::memory::MemoryLedger;

struct Frozen;
impl Clock for Frozen {
    fn now(&self) -> String {
        "2026-01-01T00:00:00Z".to_string()
    }
}

/// The workflow under test: `start → post_comment → tally`.
///
/// `post_comment` is named for an effect, because the effect is the argument.
/// `tally` is broken on the first pass and fixed by the repair.
const PARENT: &str = "review-and-tally";
const PREFIX: &str = "post_comment";
const BREAKS: &str = "tally";

fn graph() -> WorkflowGraph {
    let tool = |id: &str, slug: &str| Node {
        id: id.to_string(),
        kind: NodeKind::ToolCall,
        type_version: 1,
        name: id.to_string(),
        config: json!({ "slug": slug, "args": {} }),
        ports: Vec::new(),
        position: None,
    };
    WorkflowGraph {
        schema_version: 1,
        id: Some(PARENT.to_string()),
        name: "Review and tally".to_string(),
        inputs: Vec::new(),
        agents: Vec::new(),
        nodes: vec![
            Node {
                id: "start".to_string(),
                kind: NodeKind::Trigger,
                type_version: 1,
                name: "start".to_string(),
                config: json!({ "trigger_kind": "manual" }),
                ports: Vec::new(),
                position: None,
            },
            tool(PREFIX, PREFIX),
            // The broken step: it calls a slug that is down. The repair points
            // it at one that is not — an edit to the failed node and nothing
            // else, which is exactly what the gate is meant to allow.
            tool(BREAKS, "tally_broken"),
        ],
        edges: vec![
            Edge {
                from_node: "start".to_string(),
                from_port: "main".to_string(),
                to_node: PREFIX.to_string(),
                to_port: "main".to_string(),
            },
            Edge {
                from_node: PREFIX.to_string(),
                from_port: "main".to_string(),
                to_node: BREAKS.to_string(),
                to_port: "main".to_string(),
            },
        ],
    }
}

/// Counts every tool call, and fails `tally_broken` always.
#[derive(Default)]
struct CountingTools {
    calls: Mutex<HashMap<String, usize>>,
}

impl CountingTools {
    fn calls(&self, slug: &str) -> usize {
        self.calls
            .lock()
            .expect("lock")
            .get(slug)
            .copied()
            .unwrap_or(0)
    }
}

#[async_trait]
impl ToolInvoker for CountingTools {
    async fn invoke(&self, slug: &str, _args: Value, _conn: Option<&str>) -> EngineResult<Value> {
        *self
            .calls
            .lock()
            .expect("lock")
            .entry(slug.to_string())
            .or_insert(0) += 1;
        if slug == "tally_broken" {
            return Err(EngineError::Capability("tally_broken is down".to_string()));
        }
        Ok(json!({ "slug": slug, "ok": true }))
    }
}

/// A scripted model: refuses the first result naming the broken node, proposes
/// exactly one repair, then accepts.
struct Scripted {
    judged: Mutex<usize>,
}

impl Scripted {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            judged: Mutex::new(0),
        })
    }
}

#[async_trait]
impl LlmProvider for Scripted {
    async fn complete(&self, request: Value, _conn: Option<&str>) -> EngineResult<Value> {
        let prompt = request["messages"]
            .as_array()
            .and_then(|m| m.last())
            .and_then(|m| m["content"].as_str())
            .unwrap_or_default()
            .to_string();
        Ok(match request["tier"].as_str().unwrap_or_default() {
            // Pick whatever is offered, preferring a variant over the parent —
            // the chooser is being stood in for, and what matters is that the
            // second attempt lands on the workflow the repair produced.
            "select" => {
                let offered = offered_ids(&prompt);
                let chosen = offered
                    .iter()
                    .find(|id| *id != PARENT)
                    .or_else(|| offered.first());
                json!({ "workflow_id": chosen, "why": "it is the job", "inputs": {} })
            }
            "judge" => {
                let mut judged = self.judged.lock().expect("lock");
                *judged += 1;
                if *judged == 1 {
                    // Names the node, which is what makes `graph_is_suspect`
                    // treat this as a graph fault rather than poor work.
                    json!({
                        "satisfied": false, "blocker": "goal_not_met",
                        "gap": format!("the `{BREAKS}` step errored, so nothing was tallied"),
                        "advanced": false
                    })
                } else {
                    json!({ "satisfied": true, "blocker": "none", "gap": "", "advanced": true })
                }
            }
            // The repair, and the whole point of the staging: it edits ONLY the
            // node that failed, so the gate must allow the continue.
            "repair" => json!({
                "why": "point the tally at the slug that answers",
                "ops": [{
                    "op": "update_node_config",
                    "id": BREAKS,
                    "config": { "slug": "tally" }
                }]
            }),
            "consolidate" => json!({ "lessons": [], "corroborate": [] }),
            "generalise" => json!({ "keep": false, "why": "one-off" }),
            _ => json!({}),
        })
    }
}

/// The workflow ids a selection prompt offered, read back off the listing.
fn offered_ids(prompt: &str) -> Vec<String> {
    prompt
        .lines()
        .filter_map(|line| line.trim().strip_prefix("- id: "))
        .map(|id| id.trim().to_string())
        .collect()
}

/// A runner that keeps checkpoints — the only kind that can continue anything.
///
/// Deliberately the smallest thing that is still *real*: it compiles the
/// attempt's graph, runs or continues it through the engine against a live
/// checkpointer, and reports the boundary back. What a production host adds on
/// top (policy, records, deadlines) is not what this test is about.
struct Checkpointed {
    caps: Capabilities,
    checkpointer: Arc<InMemoryCheckpointer<Value>>,
    /// Every thread this runner started, so the test can see that a continued
    /// attempt reused one rather than opening another.
    threads: Mutex<Vec<String>>,
}

#[async_trait]
impl Runner for Checkpointed {
    async fn run(&self, attempt: &Attempt) -> Ran {
        let compiled = match compile(&attempt.graph) {
            Ok(compiled) => compiled,
            Err(err) => {
                return RunReport {
                    failed: Some(err.to_string()),
                    ..RunReport::default()
                }
                .into_ran(&attempt.graph);
            }
        };
        // A continued attempt keeps the broken run's thread: it is that run,
        // one leg later.
        let thread = match &attempt.resume {
            Some(point) => point.thread.clone(),
            None => format!("thread-{}", self.threads.lock().expect("lock").len() + 1),
        };
        self.threads.lock().expect("lock").push(thread.clone());

        let checkpointer = self.checkpointer.clone();
        let outcome = match &attempt.resume {
            Some(_) => retry_with_checkpointer(&compiled, &self.caps, checkpointer, &thread).await,
            None => {
                run_with_checkpointer(
                    &compiled,
                    RunInput::new(json!({})),
                    &self.caps,
                    checkpointer,
                    &thread,
                )
                .await
            }
        };

        let failed = outcome.as_ref().err().map(ToString::to_string);
        let boundary = failure_boundary(&(self.checkpointer.clone() as _), &thread)
            .await
            .expect("the checkpointer is readable");

        // Steps from the run state on success, and from the failure
        // boundary's committed state otherwise — a failed run still did
        // whatever it did before it broke, and a report that says nothing
        // happened is settled mechanically as terminal MissingEvidence before
        // the judge is ever asked. (The same defect, in the same shape, that
        // a real host hit: a report of "" means "looked and saw nothing".)
        let state = match &outcome {
            Ok(out) => Some(out.output.clone()),
            Err(_) => {
                tinyflows::engine::Checkpointer::get(self.checkpointer.as_ref(), &thread, None)
                    .await
                    .ok()
                    .flatten()
                    .map(|checkpoint| checkpoint.state)
            }
        };
        let steps = state
            .as_ref()
            .and_then(|state| state.get("nodes"))
            .and_then(Value::as_object)
            .map(|nodes| {
                nodes
                    .iter()
                    .map(|(id, slot)| StepRecord {
                        node_id: id.clone(),
                        status: if boundary.as_ref().is_some_and(|b| &b.failed_node == id) {
                            StepOutcome::Error
                        } else {
                            StepOutcome::Success
                        },
                        output: slot.clone(),
                        duration_ms: 0,
                        null_bindings: Vec::new(),
                        transcript: Vec::new(),
                    })
                    .collect()
            })
            .unwrap_or_default();

        let mut ran = RunReport {
            steps,
            changed: match &failed {
                None => "the tally was written".to_string(),
                Some(_) => "unknown — the run broke partway, so what its earlier steps \
                            already did is not established"
                    .to_string(),
            },
            failed,
            ..RunReport::default()
        }
        .into_ran(&attempt.graph);

        ran.resume = boundary.map(|boundary| ResumePoint {
            thread,
            failed_node: boundary.failed_node,
            workflow: attempt
                .graph
                .id
                .clone()
                .unwrap_or_else(|| PARENT.to_string()),
        });
        ran
    }
}

fn store(tag: &str) -> Arc<dyn WorkflowStore> {
    let root = std::env::temp_dir().join(format!("adaptive-continue-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("workflows")).expect("temp dir");
    let store = FileWorkflowStore::new(vec![root.join("workflows")], root.join("runs"));
    store
        .save(&WorkflowRecord {
            id: PARENT.to_string(),
            name: "Review and tally".to_string(),
            description: "post the review, then tally what it found".to_string(),
            enabled: true,
            defaults: Default::default(),
            graph: graph(),
            source_path: None,
        })
        .expect("seed the store");
    Arc::new(store)
}

#[tokio::test]
async fn a_repaired_run_continues_from_the_break_without_redoing_the_prefix() {
    let tools = Arc::new(CountingTools::default());
    let caps = Capabilities {
        llm: Scripted::new(),
        tools: tools.clone(),
        ..mock_capabilities()
    };
    let runner = Checkpointed {
        caps: Capabilities {
            tools: tools.clone(),
            ..mock_capabilities()
        },
        checkpointer: Arc::new(InMemoryCheckpointer::new()),
        threads: Mutex::new(Vec::new()),
    };
    let ledger = MemoryLedger::new();
    let store = store("repaired");
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
            "ep-continue",
            &Goal::new("review the PR and tally the findings"),
        )
        .await
        .expect("the episode runs");

    assert!(
        finished.verdict.satisfied,
        "the repaired attempt should have satisfied the goal: {:?}",
        finished.status
    );
    assert_eq!(finished.attempts, 2, "one break, one continue");

    // THE POINT. The prefix posted once, across a failed attempt and the
    // continue that followed it. Re-running from the trigger would post twice,
    // and no state comparison would notice.
    assert_eq!(
        tools.calls(PREFIX),
        1,
        "the effectful prefix must not be performed a second time"
    );
    assert_eq!(
        tools.calls("tally_broken"),
        1,
        "the broken slug was called once, on the attempt that broke"
    );
    assert_eq!(
        tools.calls("tally"),
        1,
        "and the repaired node ran once, on the continue"
    );

    // Both legs ran under one thread, which is what "continued" means here.
    let threads = runner.threads.lock().expect("lock").clone();
    assert_eq!(threads.len(), 2, "two attempts");
    assert_eq!(
        threads[0], threads[1],
        "the second attempt continued the first run rather than opening another"
    );
}

#[tokio::test]
async fn an_unsafe_repair_starts_over_and_the_prefix_runs_again() {
    // The other half, and the one that proves the gate is load-bearing rather
    // than decorative: the same episode, with a repair that edits the node
    // UPSTREAM of the failure. Continuing would re-enter the tail on a prefix
    // the new graph would never have produced, so the loop must not — and the
    // observable cost of not doing it is the prefix running twice.
    struct EditsTheUpstream {
        inner: Arc<Scripted>,
    }
    #[async_trait]
    impl LlmProvider for EditsTheUpstream {
        async fn complete(&self, request: Value, conn: Option<&str>) -> EngineResult<Value> {
            if request["tier"].as_str() == Some("repair") {
                return Ok(json!({
                    "why": "the comment it posts is what the tally reads",
                    "ops": [
                        { "op": "update_node_config", "id": PREFIX,
                          "config": { "args": { "verbose": true } } },
                        { "op": "update_node_config", "id": BREAKS,
                          "config": { "slug": "tally" } }
                    ]
                }));
            }
            self.inner.complete(request, conn).await
        }
    }

    let tools = Arc::new(CountingTools::default());
    let caps = Capabilities {
        llm: Arc::new(EditsTheUpstream {
            inner: Scripted::new(),
        }),
        tools: tools.clone(),
        ..mock_capabilities()
    };
    let runner = Checkpointed {
        caps: Capabilities {
            tools: tools.clone(),
            ..mock_capabilities()
        },
        checkpointer: Arc::new(InMemoryCheckpointer::new()),
        threads: Mutex::new(Vec::new()),
    };
    let ledger = MemoryLedger::new();
    let store = store("unsafe");
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
        .run(
            "ep-unsafe",
            &Goal::new("review the PR and tally the findings"),
        )
        .await
        .expect("the episode runs");

    assert_eq!(
        tools.calls(PREFIX),
        2,
        "an edit upstream of the failure means starting over, prefix and all"
    );
    let threads = runner.threads.lock().expect("lock").clone();
    assert_ne!(
        threads[0], threads[1],
        "a fresh run means a fresh thread — nothing was continued"
    );
}
