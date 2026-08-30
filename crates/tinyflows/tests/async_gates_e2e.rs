#![cfg(feature = "mock")]
//! End-to-end tests for the async pair: `spawn` starts work without blocking,
//! `gate` collects it on a release policy.
//!
//! The point of these nodes is *overlap*, and overlap does not show up in a
//! final state — a graph that ran everything sequentially computes the same
//! answer. So the tests here measure timing and invocation counts, not just
//! output, and each one says which of those it is actually pinning.
//!
//! Every run is wrapped in a timeout: a gate that never releases hangs rather
//! than fails, and a hung test takes the suite with it.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::{Value, json};

use tinyflows::caps::mock::mock_capabilities;
use tinyflows::caps::{TaskRunner, TaskSpec, TaskState};
use tinyflows::compiler::compile;
use tinyflows::engine::run;
use tinyflows::model::{Edge, Node, NodeKind, WorkflowGraph};

/// How long any run here may take before it is called a hang.
const GUARD: Duration = Duration::from_secs(20);

fn node(id: &str, kind: NodeKind, config: Value) -> Node {
    Node {
        id: id.to_string(),
        kind,
        type_version: 1,
        name: id.to_string(),
        config,
        ports: vec![],
        position: None,
    }
}

fn edge(from: &str, to: &str) -> Edge {
    Edge {
        from_node: from.to_string(),
        from_port: "main".to_string(),
        to_node: to.to_string(),
        to_port: "main".to_string(),
    }
}

/// A runner whose tasks settle only after being polled `settle_after` times.
///
/// Real background work does not finish the instant a gate looks at it. Forcing
/// several polls is what actually exercises the wait loop — with a runner that
/// settles immediately, a broken gate that released on its first activation
/// would pass every test here.
struct SlowRunner {
    settle_after: usize,
    polls: Mutexed,
    started: AtomicUsize,
}

type Mutexed = std::sync::Mutex<std::collections::HashMap<String, usize>>;

impl SlowRunner {
    fn new(settle_after: usize) -> Arc<Self> {
        Arc::new(Self {
            settle_after,
            polls: std::sync::Mutex::new(std::collections::HashMap::new()),
            started: AtomicUsize::new(0),
        })
    }
}

#[async_trait::async_trait]
impl TaskRunner for SlowRunner {
    async fn start(&self, spec: TaskSpec) -> tinyflows::error::Result<String> {
        let index = self.started.fetch_add(1, Ordering::SeqCst);
        let _ = spec;
        Ok(format!("t{index}"))
    }

    async fn poll(&self, ticket: &str) -> tinyflows::error::Result<TaskState> {
        let mut polls = self.polls.lock().expect("poll table poisoned");
        let count = polls.entry(ticket.to_string()).or_insert(0);
        *count += 1;
        if *count >= self.settle_after {
            Ok(TaskState::Done(json!({ "ticket": ticket })))
        } else {
            Ok(TaskState::Running)
        }
    }

    async fn cancel(&self, _ticket: &str) -> tinyflows::error::Result<()> {
        Ok(())
    }
}

/// `trigger -> spawn -> gate`, with the gate's config under test.
fn spawn_gate_graph(gate_config: Value) -> WorkflowGraph {
    let mut config = gate_config;
    config["from"] = json!(["kick"]);
    WorkflowGraph {
        name: "spawn_gate".to_string(),
        nodes: vec![
            node(
                "t",
                NodeKind::Trigger,
                // Polls cost super-steps, so the run needs headroom for them.
                json!({ "recursion_limit": 400, "max_node_visits": 300 }),
            ),
            node(
                "kick",
                NodeKind::Spawn,
                json!({ "target": "tool", "slug": "work.run" }),
            ),
            node("collect", NodeKind::Gate, config),
        ],
        edges: vec![edge("t", "kick"), edge("kick", "collect")],
        ..Default::default()
    }
}

async fn run_guarded(
    graph: &WorkflowGraph,
    caps: &tinyflows::caps::Capabilities,
) -> tinyflows::error::Result<tinyflows::engine::RunOutcome> {
    let compiled = compile(graph).expect("compile");
    match tokio::time::timeout(GUARD, run(&compiled, json!({}), caps)).await {
        Err(_) => panic!("run hung past {GUARD:?} — a gate never released"),
        Ok(inner) => inner,
    }
}

/// A gate polls until its work settles, then emits the result.
///
/// The runner deliberately reports `Running` for the first few polls, so this
/// pins the wait loop rather than a gate that happens to release immediately.
#[tokio::test]
async fn a_gate_polls_until_the_spawned_work_settles() {
    let runner = SlowRunner::new(3);
    let mut caps = mock_capabilities();
    caps.tasks = Some(runner.clone());

    let outcome = run_guarded(&spawn_gate_graph(json!({ "poll_interval_ms": 1 })), &caps)
        .await
        .expect("the gate should release once its task settles");

    let items = outcome.output["nodes"]["collect"]["items"]
        .as_array()
        .expect("the gate emitted items");
    assert_eq!(items.len(), 1, "one spawned task, one result");
    assert_eq!(items[0]["json"]["ticket"], "t0");

    let polls = outcome.output["nodes"]["collect"]["polls"]
        .as_u64()
        .expect("the gate records its poll count");
    assert!(
        polls >= 3,
        "the gate should have polled at least until the task settled, got {polls}"
    );
}

/// The spawn does not block: its branch continues before the work settles.
///
/// This is the property the whole node pair exists for, and it is invisible in
/// the final state — so it is measured by *when* the sibling ran, using the
/// runner's poll count as the clock.
#[tokio::test]
async fn a_spawn_does_not_block_its_branch() {
    let runner = SlowRunner::new(5);
    let mut caps = mock_capabilities();
    caps.tasks = Some(runner.clone());

    // `kick` spawns; `sibling` runs downstream of it and must not have waited
    // for the spawned work. If spawn blocked, the task would already be settled
    // (5 polls consumed) by the time `sibling` ran.
    let graph = WorkflowGraph {
        name: "spawn_is_non_blocking".to_string(),
        nodes: vec![
            node("t", NodeKind::Trigger, json!({ "recursion_limit": 400 })),
            node(
                "kick",
                NodeKind::Spawn,
                json!({ "target": "tool", "slug": "work.run" }),
            ),
            node("sibling", NodeKind::OutputParser, Value::Null),
        ],
        edges: vec![edge("t", "kick"), edge("kick", "sibling")],
        ..Default::default()
    };

    let outcome = run_guarded(&graph, &caps).await.expect("run");
    assert!(
        !outcome.output["nodes"]["sibling"]["items"].is_null(),
        "the branch continued past the spawn"
    );
    let polls = runner.polls.lock().expect("poll table poisoned");
    assert!(
        polls.is_empty(),
        "nothing polled the task, because nothing waited for it — spawn returned \
         a ticket rather than a result"
    );
}

/// `release: "quorum"` proceeds once `n` results are in and leaves the
/// stragglers running, rather than waiting for every task.
#[tokio::test]
async fn a_quorum_gate_releases_before_every_task_settles() {
    // Three spawns; each settles after a different number of polls, so they
    // genuinely finish at different times.
    struct Staggered {
        polls: Mutexed,
    }

    #[async_trait::async_trait]
    impl TaskRunner for Staggered {
        async fn start(&self, _spec: TaskSpec) -> tinyflows::error::Result<String> {
            let mut polls = self.polls.lock().expect("poisoned");
            let ticket = format!("t{}", polls.len());
            polls.insert(ticket.clone(), 0);
            Ok(ticket)
        }
        async fn poll(&self, ticket: &str) -> tinyflows::error::Result<TaskState> {
            let mut polls = self.polls.lock().expect("poisoned");
            let count = polls.entry(ticket.to_string()).or_insert(0);
            *count += 1;
            // `t0` settles at once; `t1` and `t2` take much longer.
            let needed = match ticket {
                "t0" => 1,
                "t1" => 2,
                _ => 50,
            };
            if *count >= needed {
                Ok(TaskState::Done(json!({ "ticket": ticket })))
            } else {
                Ok(TaskState::Running)
            }
        }
        async fn cancel(&self, _ticket: &str) -> tinyflows::error::Result<()> {
            Ok(())
        }
    }

    let runner = Arc::new(Staggered {
        polls: std::sync::Mutex::new(std::collections::HashMap::new()),
    });
    let mut caps = mock_capabilities();
    caps.tasks = Some(runner.clone());

    let graph = WorkflowGraph {
        name: "quorum".to_string(),
        nodes: vec![
            node(
                "t",
                NodeKind::Trigger,
                json!({ "recursion_limit": 400, "max_node_visits": 300 }),
            ),
            node(
                "a",
                NodeKind::Spawn,
                json!({ "target": "tool", "slug": "a" }),
            ),
            node(
                "b",
                NodeKind::Spawn,
                json!({ "target": "tool", "slug": "b" }),
            ),
            node(
                "c",
                NodeKind::Spawn,
                json!({ "target": "tool", "slug": "c" }),
            ),
            node(
                "collect",
                NodeKind::Gate,
                json!({
                    "from": ["a", "b", "c"],
                    "release": "quorum",
                    "n": 2,
                    "poll_interval_ms": 1
                }),
            ),
        ],
        edges: vec![
            edge("t", "a"),
            edge("t", "b"),
            edge("t", "c"),
            edge("a", "collect"),
            edge("b", "collect"),
            edge("c", "collect"),
        ],
        ..Default::default()
    };

    let outcome = run_guarded(&graph, &caps).await.expect("run");
    let items = outcome.output["nodes"]["collect"]["items"]
        .as_array()
        .expect("items");
    assert_eq!(
        items.len(),
        2,
        "a quorum of 2 emits exactly the two that arrived, not all three"
    );
    // The straggler is left running rather than waited for — which is the whole
    // point of a quorum, and the reason the run finishes quickly.
    let polls = runner.polls.lock().expect("poisoned");
    assert!(
        polls.get("t2").copied().unwrap_or(0) < 50,
        "the gate should not have waited for the straggler to settle"
    );
}

/// Without a `TaskRunner` the graph still runs: `spawn` performs its work inline
/// and the gate collects an already-settled ticket.
///
/// Losing the overlap is acceptable; losing the answer is not.
#[tokio::test]
async fn spawn_and_gate_still_work_with_no_task_runner_injected() {
    let mut caps = mock_capabilities();
    caps.tasks = None;

    let outcome = run_guarded(&spawn_gate_graph(json!({})), &caps)
        .await
        .expect("the graph must still run without a TaskRunner");

    let items = outcome.output["nodes"]["collect"]["items"]
        .as_array()
        .expect("the gate still emits a result");
    assert_eq!(items.len(), 1, "the inline result reaches the gate");
    assert_eq!(
        outcome.output["nodes"]["collect"]["polls"], 1,
        "an inline result is already in hand, so the gate releases on its first \
         activation without polling"
    );
}

/// A gate that never gets its results fails naming its own budget, rather than
/// spinning until the run-level backstop reports a generic runaway.
#[tokio::test]
async fn a_gate_that_never_releases_fails_naming_its_poll_budget() {
    let runner = SlowRunner::new(usize::MAX); // never settles
    let mut caps = mock_capabilities();
    caps.tasks = Some(runner);

    let err = run_guarded(
        &spawn_gate_graph(json!({ "poll_interval_ms": 1, "max_polls": 3 })),
        &caps,
    )
    .await
    .expect_err("a gate whose work never lands must fail, not hang");

    let message = err.to_string();
    assert!(
        message.contains("collect") && message.contains("polls"),
        "the failure should name the gate and its poll budget, got: {message}"
    );
}

/// `on_timeout: "partial"` settles for what arrived instead of failing.
#[tokio::test]
async fn a_timed_out_gate_can_emit_what_arrived() {
    let runner = SlowRunner::new(usize::MAX);
    let mut caps = mock_capabilities();
    caps.tasks = Some(runner);

    let outcome = run_guarded(
        &spawn_gate_graph(json!({
            "poll_interval_ms": 1,
            "max_polls": 2,
            "on_timeout": "partial"
        })),
        &caps,
    )
    .await
    .expect("a partial timeout is not a failure");

    let items = outcome.output["nodes"]["collect"]["items"]
        .as_array()
        .expect("items");
    assert!(
        items.is_empty(),
        "nothing settled, so the partial result is empty — but the run completed"
    );
}
