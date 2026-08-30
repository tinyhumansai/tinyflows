#![cfg(feature = "mock")]
//! End-to-end tests for **per-item fan-out**: one node multiplying an array of
//! input into N concurrent units of work, array in and array out.
//!
//! This is distinct from the graph-shaped fan-out covered by `parallel_e2e.rs`.
//! There, concurrency comes from authoring N sibling nodes, so the width is
//! fixed when the graph is written. Here a *single* node maps over whatever
//! array reaches it, so the width is data-driven — `split_out` → `agent` →
//! `merge` runs one agent turn per element, bounded by `config.concurrency`.
//!
//! The tests assert the three properties that make the feature usable:
//! items really do run concurrently, results come back in input order, and one
//! failing item does not discard the batch.
//!
//! Gated behind the `mock` cargo feature.

use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tinyflows::caps::mock::mock_capabilities;
use tinyflows::caps::{Capabilities, LlmProvider};
use tinyflows::compiler::compile;
use tinyflows::engine::run;
use tinyflows::model::{Edge, Node, NodeKind, TriggerKind, WorkflowGraph};
use tinyflows::observability::RunObserver;

/// Builds a node with the given id, kind, and config (no ports, no position).
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

/// Builds an edge from `from_node`'s `main` port into `to_node`'s `main` port.
fn edge(from_node: &str, to_node: &str) -> Edge {
    Edge {
        from_node: from_node.to_string(),
        from_port: "main".to_string(),
        to_node: to_node.to_string(),
        to_port: "main".to_string(),
    }
}

/// An LLM stand-in that records how many completions overlap, so a test can
/// prove work actually ran concurrently rather than merely finishing.
///
/// Each call registers itself, yields enough times for its peers to start, then
/// echoes the request's prompt. `peak` is the high-water mark of simultaneous
/// calls — 1 means the batch ran strictly sequentially.
#[derive(Default)]
struct ConcurrencyProbe {
    live: AtomicUsize,
    peak: AtomicUsize,
    calls: AtomicUsize,
    /// A prompt that must fail, to exercise the collect policy.
    fail_on: Option<String>,
}

impl ConcurrencyProbe {
    fn peak(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl LlmProvider for ConcurrencyProbe {
    async fn complete(
        &self,
        request: Value,
        _conn: Option<&str>,
    ) -> tinyflows::error::Result<Value> {
        let live = self.live.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(live, Ordering::SeqCst);
        self.calls.fetch_add(1, Ordering::SeqCst);
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        self.live.fetch_sub(1, Ordering::SeqCst);

        let prompt = request
            .get("prompt")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if self.fail_on.as_deref() == Some(prompt.as_str()) {
            return Err(tinyflows::error::EngineError::Capability(format!(
                "probe: refusing {prompt}"
            )));
        }
        Ok(json!({ "text": prompt }))
    }
}

fn caps_with(probe: Arc<ConcurrencyProbe>) -> Capabilities {
    Capabilities {
        llm: probe,
        ..mock_capabilities()
    }
}

/// `trigger → split_out(topics) → agent(per_item) → merge`, with the agent's
/// prompt bound to the current item. This is the canonical fan-out shape.
fn fanout_graph(agent_config: Value) -> WorkflowGraph {
    WorkflowGraph {
        nodes: vec![
            node(
                "t",
                NodeKind::Trigger,
                json!({ "kind": TriggerKind::Manual }),
            ),
            node("split", NodeKind::SplitOut, json!({ "path": "topics" })),
            node("work", NodeKind::Agent, agent_config),
            node("join", NodeKind::Merge, Value::Null),
        ],
        edges: vec![
            edge("t", "split"),
            edge("split", "work"),
            edge("work", "join"),
        ],
        ..Default::default()
    }
}

/// The trigger payload: five topics to fan out over. Each element is an object
/// so the agent prompt can bind a named field (`=item.name`), which is how a
/// real fan-out addresses its current element.
fn topics() -> Value {
    json!({
        "topics": [
            { "name": "alpha" },
            { "name": "beta" },
            { "name": "gamma" },
            { "name": "delta" },
            { "name": "epsilon" },
        ]
    })
}

/// The `text` of every item a node emitted, in emission order.
fn texts(output: &Value, node_id: &str) -> Vec<String> {
    output["nodes"][node_id]["items"]
        .as_array()
        .expect("items array")
        .iter()
        .map(|i| i["json"]["text"].as_str().unwrap_or_default().to_string())
        .collect()
}

#[tokio::test]
async fn a_fanned_out_agent_runs_items_concurrently_and_returns_them_in_order() {
    let probe = Arc::new(ConcurrencyProbe::default());
    let graph = fanout_graph(json!({
        "prompt": "=item.name",
        "execution": "per_item",
        "concurrency": 4,
    }));
    let compiled = compile(&graph).expect("compile");
    let out = run(&compiled, topics(), &caps_with(probe.clone()))
        .await
        .expect("run");

    assert_eq!(probe.calls(), 5, "one agent turn per input item");
    assert!(
        probe.peak() > 1,
        "the whole point is concurrency; peak in-flight was {}",
        probe.peak()
    );
    assert!(
        probe.peak() <= 4,
        "must respect the concurrency bound; peak in-flight was {}",
        probe.peak()
    );
    // Array in, array out — in the original order despite finishing out of order.
    assert_eq!(
        texts(&out.output, "work"),
        ["alpha", "beta", "gamma", "delta", "epsilon"]
    );
    // ...and the merge downstream sees the whole array.
    assert_eq!(
        out.output["nodes"]["join"]["items"]
            .as_array()
            .expect("merged items")
            .len(),
        5
    );
}

#[tokio::test]
async fn concurrency_all_runs_every_item_at_once() {
    let probe = Arc::new(ConcurrencyProbe::default());
    let graph = fanout_graph(json!({
        "prompt": "=item.name",
        "execution": "per_item",
        "concurrency": "all",
    }));
    let compiled = compile(&graph).expect("compile");
    run(&compiled, topics(), &caps_with(probe.clone()))
        .await
        .expect("run");

    assert_eq!(probe.peak(), 5, "`\"all\"` means every item at once");
}

#[tokio::test]
async fn without_concurrency_the_same_graph_stays_sequential() {
    // Back-compat guard: opting into `per_item` alone must not change timing.
    // If this ever reports a peak above 1, fan-out has become the default and
    // every existing workflow silently changed its concurrency profile.
    let probe = Arc::new(ConcurrencyProbe::default());
    let graph = fanout_graph(json!({
        "prompt": "=item.name",
        "execution": "per_item",
    }));
    let compiled = compile(&graph).expect("compile");
    run(&compiled, topics(), &caps_with(probe.clone()))
        .await
        .expect("run");

    assert_eq!(probe.calls(), 5);
    assert_eq!(probe.peak(), 1, "unset concurrency must stay sequential");
}

#[tokio::test]
async fn one_failing_item_does_not_discard_the_rest_of_the_batch() {
    let probe = Arc::new(ConcurrencyProbe {
        fail_on: Some("gamma".to_string()),
        ..Default::default()
    });
    let graph = fanout_graph(json!({
        "prompt": "=item.name",
        "execution": "per_item",
        "concurrency": 4,
    }));
    let compiled = compile(&graph).expect("compile");
    let out = run(&compiled, topics(), &caps_with(probe.clone()))
        .await
        .expect("a fanned-out batch collects item failures instead of failing the run");

    let items = out.output["nodes"]["work"]["items"]
        .as_array()
        .expect("items array");
    assert_eq!(items.len(), 5, "one output per input, failures included");

    // The failed slot is marked in place, so the good results survive and a
    // downstream `condition` can branch on `=item.json.failed`. Each stored
    // entry is a serialized Item, so `["json"]` is the envelope and
    // `["json"]["json"]` is what `=item.json` resolves to.
    assert_eq!(items[2]["json"]["json"]["failed"], true);
    assert!(
        items[2]["json"]["json"]["error"]
            .as_str()
            .expect("error message")
            .contains("gamma")
    );
    for good in [0, 1, 3, 4] {
        assert!(
            items[good]["json"]["json"]["failed"].is_null(),
            "item {good} should have succeeded"
        );
    }
    // The surviving results are still the right ones, in the right slots.
    assert_eq!(items[0]["json"]["text"], "alpha");
    assert_eq!(items[4]["json"]["text"], "epsilon");
}

#[tokio::test]
async fn a_fanned_out_batch_can_opt_back_into_failing_the_node() {
    // `on_item_error: fail_fast` restores the sequential contract: the error
    // reaches the node, so the node's own `on_error` policy governs the run.
    let probe = Arc::new(ConcurrencyProbe {
        fail_on: Some("gamma".to_string()),
        ..Default::default()
    });
    let graph = fanout_graph(json!({
        "prompt": "=item.name",
        "execution": "per_item",
        "concurrency": 4,
        "on_item_error": "fail_fast",
    }));
    let compiled = compile(&graph).expect("compile");
    let err = run(&compiled, topics(), &caps_with(probe))
        .await
        .expect_err("fail_fast must surface the item error as a node failure");
    assert!(
        err.to_string().contains("gamma"),
        "expected the failing item's error, got: {err}"
    );
}

#[derive(Default)]
struct ItemObserver {
    starts: std::sync::Mutex<Vec<(String, usize, usize)>>,
    finishes: std::sync::Mutex<Vec<(String, usize, usize, bool)>>,
    live: AtomicUsize,
    peak_live: AtomicUsize,
}

impl RunObserver for ItemObserver {
    fn on_item_start(&self, node_id: &str, index: usize, total: usize) {
        self.starts
            .lock()
            .unwrap()
            .push((node_id.to_string(), index, total));
        let live = self.live.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak_live.fetch_max(live, Ordering::SeqCst);
    }

    fn on_item_finish(&self, node_id: &str, index: usize, total: usize, ok: bool) {
        self.live.fetch_sub(1, Ordering::SeqCst);
        self.finishes
            .lock()
            .unwrap()
            .push((node_id.to_string(), index, total, ok));
    }
}

#[tokio::test]
async fn a_real_run_reports_each_fanned_out_item_to_the_observer() {
    let watcher = Arc::new(ItemObserver::default());
    let observer: Arc<dyn RunObserver> = watcher.clone();
    let graph = fanout_graph(json!({
        "prompt": "=item.name",
        "execution": "per_item",
        "concurrency": 3,
    }));
    let compiled = compile(&graph).expect("compile");

    tinyflows::engine::run_with_observer(
        &compiled,
        topics(),
        &caps_with(Arc::new(ConcurrencyProbe::default())),
        &observer,
    )
    .await
    .expect("run");

    let starts = watcher.starts.lock().unwrap().clone();
    let finishes = watcher.finishes.lock().unwrap().clone();
    assert_eq!(starts.len(), 5);
    assert_eq!(finishes.len(), 5);
    assert!(
        starts
            .iter()
            .all(|(node, _, total)| node == "work" && *total == 5)
    );
    assert!(
        finishes
            .iter()
            .all(|(node, _, total, ok)| node == "work" && *total == 5 && *ok)
    );
    let mut indices = starts
        .iter()
        .map(|(_, index, _)| *index)
        .collect::<Vec<_>>();
    indices.sort_unstable();
    assert_eq!(indices, [0, 1, 2, 3, 4]);
    let peak = watcher.peak_live.load(Ordering::SeqCst);
    assert!(peak > 1 && peak <= 3, "unexpected live-worker peak: {peak}");
}
