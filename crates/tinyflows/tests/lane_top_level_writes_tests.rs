#![cfg(feature = "mock")]
//! A lane activation writes only `nodes.<id>.lanes.<lane>`, never the node's
//! top-level `nodes.<id>` slot — on every path out of an activation, not just
//! the success path.
//!
//! Three paths used to break it: a cancel seen at the node boundary, a cancel
//! seen during retry or backoff, and a `Reenter` (a gate polling). The merge
//! reducer never deletes a key, so a top-level `items` absent from the final
//! state was absent at every boundary; the re-entry test checks every
//! checkpoint anyway, because there the question is also *when* the gather
//! released.

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tinyflows::caps::mock::mock_capabilities;
use tinyflows::caps::{TaskRunner, TaskSpec, TaskState, ToolInvoker};
use tinyflows::compiler::compile;
use tinyflows::engine::{
    CancellationToken, Checkpointer, InMemoryCheckpointer, run_cancellable, run_with_checkpointer,
};
use tinyflows::error::EngineError;
use tinyflows::model::{Edge, Node, NodeKind, WorkflowGraph};

const GUARD: Duration = Duration::from_secs(10);

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

/// `trigger -> scatter -> body[0] -> … -> gather`, the gather collecting the
/// last body node.
fn lane_graph(body: Vec<Node>) -> WorkflowGraph {
    let last = body.last().map(|n| n.id.clone()).unwrap_or_default();
    let mut nodes = vec![
        node(
            "trigger",
            NodeKind::Trigger,
            json!({ "recursion_limit": 200 }),
        ),
        node("scatter", NodeKind::Scatter, json!({ "path": "rows" })),
    ];
    let mut path = vec!["trigger".to_string(), "scatter".to_string()];
    path.extend(body.iter().map(|n| n.id.clone()));
    path.push("gather".to_string());
    let edges = path
        .windows(2)
        .map(|pair| edge(&pair[0], &pair[1]))
        .collect();
    nodes.extend(body);
    nodes.push(node(
        "gather",
        NodeKind::Gather,
        json!({ "from": [last], "poll_interval_ms": 1 }),
    ));
    WorkflowGraph {
        name: "lane_top_level_writes".to_string(),
        nodes,
        edges,
        ..Default::default()
    }
}

fn rows(count: usize) -> Value {
    json!({ "rows": (0..count).map(|index| json!({ "index": index })).collect::<Vec<_>>() })
}

/// Fails when any of `ids` has a top-level `items` — a write only a non-lane
/// activation may make, and every activation of these nodes is in a lane.
fn assert_no_top_level_items(state: &Value, ids: &[&str], when: &str) {
    for id in ids {
        assert!(
            state["nodes"][id].get("items").is_none(),
            "{when}: lane body node {id:?} has a top-level slot: {}",
            state["nodes"][id]
        );
    }
}

fn lane_slots<'a>(state: &'a Value, id: &str) -> Vec<&'a Value> {
    state["nodes"][id]["lanes"]
        .as_object()
        .map(|lanes| lanes.values().collect())
        .unwrap_or_default()
}

fn statuses(state: &Value, id: &str) -> Vec<String> {
    lane_slots(state, id)
        .iter()
        .map(|slot| slot["status"].as_str().unwrap_or("<none>").to_string())
        .collect()
}

/// Starts work that never finishes and cancels the run on the first start.
struct CancellingRunner {
    token: CancellationToken,
    started: Mutex<Vec<String>>,
    cancelled: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl TaskRunner for CancellingRunner {
    async fn start(&self, _spec: TaskSpec) -> tinyflows::error::Result<String> {
        let mut started = self.started.lock().expect("started mutex poisoned");
        let ticket = format!("ticket-{}", started.len());
        started.push(ticket.clone());
        self.token.cancel();
        Ok(ticket)
    }

    async fn poll(&self, _ticket: &str) -> tinyflows::error::Result<TaskState> {
        Ok(TaskState::Running)
    }

    async fn cancel(&self, ticket: &str) -> tinyflows::error::Result<()> {
        self.cancelled
            .lock()
            .expect("cancelled mutex poisoned")
            .push(ticket.to_string());
        Ok(())
    }
}

/// (a) A cancel seen at a node boundary inside a lane.
///
/// The first `spawn` cancels the run, so every `after` activation — each one
/// scheduled into its lane by a spawn that did start — no-ops at its boundary.
#[tokio::test]
async fn boundary_cancel_inside_a_lane_writes_only_the_lane_slot() {
    let graph = lane_graph(vec![
        node(
            "spawn",
            NodeKind::Spawn,
            json!({ "target": "tool", "slug": "lane.task" }),
        ),
        node(
            "after",
            NodeKind::Transform,
            json!({ "set": { "seen": true } }),
        ),
    ]);
    let compiled = compile(&graph).expect("compile");
    let token = CancellationToken::new();
    let runner = Arc::new(CancellingRunner {
        token: token.clone(),
        started: Mutex::new(Vec::new()),
        cancelled: Mutex::new(Vec::new()),
    });
    let mut caps = mock_capabilities();
    caps.tasks = Some(runner.clone());

    let run = run_cancellable(&compiled, rows(2), &caps, token);
    let outcome = tokio::time::timeout(GUARD, run)
        .await
        .expect("a cancelled lane run must settle")
        .expect("cancellation returns a partial outcome");

    assert!(outcome.cancelled);
    let state = &outcome.output;
    assert_no_top_level_items(state, &["spawn", "after"], "final state");
    let after = statuses(state, "after");
    assert!(
        !after.is_empty() && after.iter().all(|status| status == "cancelled"),
        "every lane that reached `after` was cancelled at its boundary: {after:?}"
    );
    assert!(
        lane_slots(state, "after")
            .iter()
            .all(|slot| slot["items"] == json!([])),
        "a cancelled lane carries no items"
    );
    let spawn = statuses(state, "spawn");
    assert!(
        spawn.iter().all(|s| s == "ok" || s == "cancelled"),
        "{spawn:?}"
    );

    // The cleanup still finds every ticket, now that cancelled lanes write
    // their lane slot instead of the top level.
    let started: BTreeSet<String> = runner
        .started
        .lock()
        .expect("started mutex poisoned")
        .iter()
        .cloned()
        .collect();
    let cancelled: BTreeSet<String> = runner
        .cancelled
        .lock()
        .expect("cancelled mutex poisoned")
        .iter()
        .cloned()
        .collect();
    assert!(!started.is_empty());
    assert_eq!(cancelled, started);
}

/// Fails every call, cancelling the run as it does.
struct CancelOnCall {
    token: CancellationToken,
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl ToolInvoker for CancelOnCall {
    async fn invoke(
        &self,
        _slug: &str,
        _args: Value,
        _conn: Option<&str>,
    ) -> tinyflows::error::Result<Value> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.token.cancel();
        Err(EngineError::Capability("transient failure".to_string()))
    }
}

/// (b) A cancel seen during retry (`backoff_ms: 0`, the check before the next
/// attempt) or during backoff (`backoff_ms > 0`, the check inside the wait).
/// Either way the first attempt has already failed and cancelled the run, so
/// no time is spent waiting.
#[tokio::test]
async fn cancel_during_retry_or_backoff_inside_a_lane_writes_only_the_lane_slot() {
    for backoff_ms in [0_u64, 40] {
        let graph = lane_graph(vec![
            node(
                "call",
                NodeKind::ToolCall,
                json!({
                    "slug": "lane.flaky",
                    "retry": { "max_attempts": 3, "backoff_ms": backoff_ms },
                }),
            ),
            node(
                "after",
                NodeKind::Transform,
                json!({ "set": { "seen": true } }),
            ),
        ]);
        let compiled = compile(&graph).expect("compile");
        let token = CancellationToken::new();
        let tools = Arc::new(CancelOnCall {
            token: token.clone(),
            calls: AtomicUsize::new(0),
        });
        let mut caps = mock_capabilities();
        caps.tools = tools.clone();

        let run = run_cancellable(&compiled, rows(2), &caps, token);
        let outcome = tokio::time::timeout(GUARD, run)
            .await
            .expect("a cancelled lane run must settle")
            .expect("cancellation returns a partial outcome");

        let when = format!("backoff_ms {backoff_ms}");
        assert!(outcome.cancelled, "{when}");
        assert!(
            tools.calls.load(Ordering::SeqCst) >= 1,
            "{when}: no attempt ran"
        );
        let state = &outcome.output;
        assert_no_top_level_items(state, &["call", "after"], &when);
        // Both lanes end cancelled — the one whose attempt failed, at its retry
        // check, and any other at its boundary. None is "failed": the retry
        // budget was never spent.
        assert_eq!(
            statuses(state, "call"),
            vec!["cancelled".to_string(); 2],
            "{when}"
        );
        assert!(
            state["nodes"]["after"].is_null(),
            "{when}: a cancelled lane schedules nothing: {}",
            state["nodes"]["after"]
        );
    }
}

/// Starts tickets in order; `ticket-0` reports `Running` for its first two
/// polls, every other ticket settles on its first. Each result names its
/// ticket, which is how the test reads lane order back.
#[derive(Default)]
struct SlowFirstRunner {
    started: Mutex<usize>,
    polls: Mutex<HashMap<String, usize>>,
}

#[async_trait::async_trait]
impl TaskRunner for SlowFirstRunner {
    async fn start(&self, _spec: TaskSpec) -> tinyflows::error::Result<String> {
        let mut started = self.started.lock().expect("started mutex poisoned");
        let ticket = format!("ticket-{started}");
        *started += 1;
        Ok(ticket)
    }

    async fn poll(&self, ticket: &str) -> tinyflows::error::Result<TaskState> {
        let mut polls = self.polls.lock().expect("polls mutex poisoned");
        let count = polls.entry(ticket.to_string()).or_default();
        *count += 1;
        Ok(if ticket == "ticket-0" && *count < 3 {
            TaskState::Running
        } else {
            TaskState::Done(json!({ "ticket": ticket }))
        })
    }

    async fn cancel(&self, _ticket: &str) -> tinyflows::error::Result<()> {
        Ok(())
    }
}

/// (c) A gate polling (`Reenter`) inside a lane body.
///
/// One lane's gate re-enters twice while the other settles at once. The
/// re-entry must stay in its lane, write only its lane slot, and not count as
/// an arrival — so the gather releases once, with both lanes, in lane order.
/// Run for both ways a gate names its tickets.
#[tokio::test]
async fn reenter_inside_a_lane_stays_in_the_lane_and_gathers_in_lane_order() {
    let expected_ok = vec!["ok".to_string(); 2];
    for gate in [
        json!({ "tickets": "=item.ticket", "poll_interval_ms": 1, "max_polls": 20 }),
        json!({ "from": ["spawn"], "poll_interval_ms": 1, "max_polls": 20 }),
    ] {
        let when = format!("gate {gate}");
        let graph = lane_graph(vec![
            node(
                "spawn",
                NodeKind::Spawn,
                json!({ "target": "tool", "slug": "lane.task" }),
            ),
            node("gate", NodeKind::Gate, gate),
        ]);
        let compiled = compile(&graph).expect("compile");
        let mut caps = mock_capabilities();
        caps.tasks = Some(Arc::new(SlowFirstRunner::default()));
        let checkpointer = Arc::new(InMemoryCheckpointer::<Value>::new());
        let thread = "lane-reenter";

        let run = run_with_checkpointer(&compiled, rows(2), &caps, checkpointer.clone(), thread);
        let outcome = tokio::time::timeout(GUARD, run)
            .await
            .expect("the lane run must settle")
            .expect("the lane run succeeds");

        let state = &outcome.output;
        assert_no_top_level_items(state, &["spawn", "gate"], &when);
        assert_eq!(statuses(state, "gate"), expected_ok, "{when}");
        let max_polls = lane_slots(state, "gate")
            .iter()
            .filter_map(|slot| slot["polls"].as_u64())
            .max();
        assert_eq!(
            max_polls,
            Some(3),
            "{when}: the slow lane's gate re-entered in its lane"
        );

        // Lane order, not completion order: the slow lane may be either index.
        let gathered = state["nodes"]["gather"]["items"]
            .as_array()
            .expect("gather output");
        assert_eq!(gathered.len(), 2, "{when}: {gathered:?}");
        for (index, item) in gathered.iter().enumerate() {
            let lane = format!("scatter#{index}");
            let spawned = &state["nodes"]["spawn"]["lanes"][lane.as_str()];
            let ticket = &spawned["items"][0]["json"]["ticket"];
            assert!(ticket.is_string(), "{when}: lane {index} spawned no ticket");
            assert_eq!(&item["json"]["ticket"], ticket, "{when}: lane {index}");
        }

        // Every boundary: no top-level write, and the gather never released
        // while a lane was still re-entering.
        let history = checkpointer.get_thread(thread).await.expect("checkpoints");
        assert!(!history.is_empty(), "{when}: no checkpoints were written");
        let mut saw_pending = false;
        for checkpoint in &history {
            let at = format!("{when}, checkpoint {}", checkpoint.checkpoint_id);
            assert_no_top_level_items(&checkpoint.state, &["spawn", "gate"], &at);
            let gate = statuses(&checkpoint.state, "gate");
            saw_pending |= gate.iter().any(|status| status == "pending");
            let released = checkpoint.state["nodes"]["gather"]["items"]
                .as_array()
                .is_some_and(|items| !items.is_empty());
            if released {
                assert_eq!(gate, expected_ok, "{at}: released early");
            }
        }
        assert!(saw_pending, "{when}: no pending boundary");
    }
}
