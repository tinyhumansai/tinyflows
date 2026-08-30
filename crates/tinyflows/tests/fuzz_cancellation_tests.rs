#![cfg(feature = "mock")]
//! Generated cancellation schedules for `spawn` / `gate` workflows.
//!
//! Every generated task remains running forever. The runner flips the workflow
//! cancellation token after an arbitrary start, which makes every issued
//! ticket uncollected. A correct run must settle promptly and ask the runner to
//! cancel each of those tickets.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use proptest::prelude::*;
use serde_json::{Value, json};
use tinyflows::caps::mock::mock_capabilities;
use tinyflows::caps::{TaskRunner, TaskSpec, TaskState};
use tinyflows::compiler::compile;
use tinyflows::engine::{CancellationToken, run_cancellable};
use tinyflows::model::{Edge, Node, NodeKind, WorkflowGraph};

const GUARD: Duration = Duration::from_secs(2);

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

fn spawned_graph(tasks: usize) -> WorkflowGraph {
    let mut nodes = vec![
        node(
            "trigger",
            NodeKind::Trigger,
            json!({ "recursion_limit": 100 }),
        ),
        node("fanout", NodeKind::OutputParser, Value::Null),
    ];
    let mut edges = vec![edge("trigger", "fanout")];
    let mut sources = Vec::new();
    for index in 0..tasks {
        let id = format!("spawn_{index}");
        nodes.push(node(
            &id,
            NodeKind::Spawn,
            json!({ "target": "tool", "slug": format!("task.{index}") }),
        ));
        edges.push(edge("fanout", &id));
        sources.push(id);
    }
    nodes.push(node(
        "gate",
        NodeKind::Gate,
        json!({
            "from": sources,
            "release": "all",
            "poll_interval_ms": 1,
            "max_polls": 1_000,
        }),
    ));
    for index in 0..tasks {
        edges.push(edge(&format!("spawn_{index}"), "gate"));
    }
    WorkflowGraph {
        name: "generated_cancellation".to_string(),
        nodes,
        edges,
        ..Default::default()
    }
}

fn scattered_spawn_graph() -> WorkflowGraph {
    WorkflowGraph {
        name: "generated_lane_cancellation".to_string(),
        nodes: vec![
            node(
                "trigger",
                NodeKind::Trigger,
                json!({ "recursion_limit": 100 }),
            ),
            node("scatter", NodeKind::Scatter, json!({ "path": "rows" })),
            node(
                "spawn",
                NodeKind::Spawn,
                json!({ "target": "tool", "slug": "lane.task" }),
            ),
            node(
                "gather",
                NodeKind::Gather,
                json!({ "from": ["spawn"], "poll_interval_ms": 1 }),
            ),
        ],
        edges: vec![
            edge("trigger", "scatter"),
            edge("scatter", "spawn"),
            edge("spawn", "gather"),
        ],
        ..Default::default()
    }
}

struct CancellingRunner {
    token: CancellationToken,
    cancel_after: usize,
    started: Mutex<Vec<String>>,
    cancelled: Mutex<Vec<String>>,
}

impl CancellingRunner {
    fn new(token: CancellationToken, cancel_after: usize) -> Arc<Self> {
        Arc::new(Self {
            token,
            cancel_after,
            started: Mutex::new(Vec::new()),
            cancelled: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait::async_trait]
impl TaskRunner for CancellingRunner {
    async fn start(&self, _spec: TaskSpec) -> tinyflows::error::Result<String> {
        let mut started = self.started.lock().expect("started mutex poisoned");
        let ticket = format!("ticket-{}", started.len());
        started.push(ticket.clone());
        if started.len() >= self.cancel_after {
            self.token.cancel();
        }
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

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build runtime")
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, ..ProptestConfig::default() })]

    #[test]
    fn cancellation_settles_and_cleans_up_every_uncollected_ticket(
        tasks in 1usize..9,
        fraction in 0usize..8,
    ) {
        let cancel_after = 1 + fraction % tasks;
        let compiled = compile(&spawned_graph(tasks)).expect("generated graph compiles");
        let token = CancellationToken::new();
        let runner = CancellingRunner::new(token.clone(), cancel_after);
        let mut caps = mock_capabilities();
        caps.tasks = Some(runner.clone());

        let outcome = runtime().block_on(async {
            tokio::time::timeout(
                GUARD,
                run_cancellable(&compiled, json!({}), &caps, token),
            )
            .await
            .expect("a cancelled run must settle promptly")
            .expect("cooperative cancellation returns a partial outcome")
        });

        prop_assert!(outcome.cancelled);
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
        prop_assert_eq!(
            cancelled,
            started,
            "every issued, perpetually-running ticket must be cancelled"
        );
    }

    #[test]
    fn cancellation_also_cleans_up_tickets_stored_in_lane_slots(
        rows in 1usize..9,
        fraction in 0usize..8,
    ) {
        let cancel_after = 1 + fraction % rows;
        let compiled = compile(&scattered_spawn_graph()).expect("generated graph compiles");
        let token = CancellationToken::new();
        let runner = CancellingRunner::new(token.clone(), cancel_after);
        let mut caps = mock_capabilities();
        caps.tasks = Some(runner.clone());
        let input_rows: Vec<Value> = (0..rows).map(|index| json!({ "index": index })).collect();

        let outcome = runtime().block_on(async {
            tokio::time::timeout(
                GUARD,
                run_cancellable(&compiled, json!({ "rows": input_rows }), &caps, token),
            )
            .await
            .expect("a cancelled lane run must settle promptly")
            .expect("cooperative cancellation returns a partial outcome")
        });
        prop_assert!(outcome.cancelled);

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
        prop_assert_eq!(cancelled, started);
    }
}
