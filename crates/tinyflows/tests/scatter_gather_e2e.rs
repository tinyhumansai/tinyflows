#![cfg(feature = "mock")]
//! End-to-end tests for `scatter` / `gather`.
//!
//! The claim under test is that a scatter fans out the **downstream path**, not
//! just its immediate successors: every node between the scatter and its gather
//! runs once per lane. A test that only checked the gather's output would pass
//! for a fan-out that ran the pipeline once with all the items, so these assert
//! on how many times the *intermediate* nodes ran.
//!
//! Every run is wrapped in a timeout: a gather that never releases hangs rather
//! than fails.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::{Value, json};

use tinyflows::caps::mock::mock_capabilities;
use tinyflows::compiler::compile;
use tinyflows::engine::run;
use tinyflows::model::{Edge, Node, NodeKind, WorkflowGraph};

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

/// `t -> scatter -> work -> gather`, fanning out over `rows`.
fn scatter_graph(scatter_config: Value, gather_config: Value) -> WorkflowGraph {
    let mut gather = gather_config;
    gather["from"] = json!(["work"]);
    WorkflowGraph {
        name: "scatter_gather".to_string(),
        nodes: vec![
            node(
                "t",
                NodeKind::Trigger,
                json!({ "recursion_limit": 400, "max_node_visits": 300 }),
            ),
            node("fan", NodeKind::Scatter, scatter_config),
            node(
                "work",
                NodeKind::Transform,
                json!({ "set": { "seen": "=item.v" } }),
            ),
            node("collect", NodeKind::Gather, gather),
        ],
        edges: vec![
            edge("t", "fan"),
            edge("fan", "work"),
            edge("work", "collect"),
        ],
        ..Default::default()
    }
}

async fn run_guarded(
    graph: &WorkflowGraph,
    caps: &tinyflows::caps::Capabilities,
    input: Value,
) -> tinyflows::error::Result<tinyflows::engine::RunOutcome> {
    let compiled = compile(graph).expect("compile");
    match tokio::time::timeout(GUARD, run(&compiled, input, caps)).await {
        Err(_) => panic!("run hung past {GUARD:?} — a gather never released"),
        Ok(inner) => inner,
    }
}

/// The headline behaviour: three lanes, and the lane body runs three times.
#[tokio::test]
async fn a_scatter_runs_the_downstream_path_once_per_lane() {
    let outcome = run_guarded(
        &scatter_graph(json!({ "path": "rows" }), json!({})),
        &mock_capabilities(),
        json!({ "rows": [{ "v": "a" }, { "v": "b" }, { "v": "c" }] }),
    )
    .await
    .expect("run");

    // The lane worker recorded one slot per lane, not one slot in total. This is
    // the assertion that separates a scatter from an ordinary fan-out.
    let lanes = outcome.output["nodes"]["work"]["lanes"]
        .as_object()
        .expect("the lane worker recorded per-lane slots");
    assert_eq!(lanes.len(), 3, "one lane slot per lane, got {lanes:?}");

    let items = outcome.output["nodes"]["collect"]["items"]
        .as_array()
        .expect("the gather emitted items");
    assert_eq!(items.len(), 3, "every lane's output reached the gather");
    let seen: Vec<&str> = items
        .iter()
        .filter_map(|item| item["json"]["seen"].as_str())
        .collect();
    assert_eq!(
        seen,
        vec!["a", "b", "c"],
        "results are ordered by lane index, and each lane saw only its own item"
    );
}

/// Lanes genuinely run concurrently rather than one after another.
///
/// Measured as observed overlap: a scatter that ran its lanes sequentially would
/// produce identical output.
#[tokio::test]
async fn lanes_run_concurrently() {
    struct OverlapProbe {
        in_flight: AtomicUsize,
        peak: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl tinyflows::caps::ToolInvoker for OverlapProbe {
        async fn invoke(
            &self,
            _slug: &str,
            _args: Value,
            _conn: Option<&str>,
        ) -> tinyflows::error::Result<Value> {
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            Ok(json!({ "ok": true }))
        }
    }

    let probe = Arc::new(OverlapProbe {
        in_flight: AtomicUsize::new(0),
        peak: AtomicUsize::new(0),
    });
    let mut caps = mock_capabilities();
    caps.tools = probe.clone();

    let mut graph = scatter_graph(json!({ "path": "rows" }), json!({}));
    // Swap the lane body for a capability-backed node so overlap is observable.
    graph.nodes.retain(|n| n.id != "work");
    graph.nodes.push(node(
        "work",
        NodeKind::ToolCall,
        json!({ "slug": "lane.run" }),
    ));

    run_guarded(
        &graph,
        &caps,
        json!({ "rows": [{ "v": 1 }, { "v": 2 }, { "v": 3 }, { "v": 4 }] }),
    )
    .await
    .expect("run");

    assert!(
        probe.peak.load(Ordering::SeqCst) > 1,
        "lanes must overlap; observed peak {} means they ran one at a time",
        probe.peak.load(Ordering::SeqCst)
    );
}

/// A multi-node lane carries its envelope the whole way: every node between the
/// scatter and the gather runs per lane, not just the first.
#[tokio::test]
async fn a_lane_spanning_several_nodes_runs_each_of_them_per_lane() {
    let mut graph = scatter_graph(json!({ "path": "rows" }), json!({}));
    graph.nodes.push(node(
        "second",
        NodeKind::Transform,
        json!({ "set": { "stage": 2 } }),
    ));
    // Re-wire: fan -> work -> second -> collect.
    graph
        .edges
        .retain(|e| !(e.from_node == "work" && e.to_node == "collect"));
    graph.edges.push(edge("work", "second"));
    graph.edges.push(edge("second", "collect"));
    for n in &mut graph.nodes {
        if n.id == "collect" {
            n.config["from"] = json!(["second"]);
        }
    }

    let outcome = run_guarded(
        &graph,
        &mock_capabilities(),
        json!({ "rows": [{ "v": "a" }, { "v": "b" }] }),
    )
    .await
    .expect("run");

    for id in ["work", "second"] {
        let lanes = outcome.output["nodes"][id]["lanes"]
            .as_object()
            .unwrap_or_else(|| panic!("{id} should have per-lane slots"));
        assert_eq!(lanes.len(), 2, "{id} ran once per lane");
    }
    assert_eq!(
        outcome.output["nodes"]["collect"]["items"]
            .as_array()
            .map(Vec::len),
        Some(2),
        "both lanes reached the gather through the two-node body"
    );
}

/// Lane slots never clobber each other, and never write the top-level slot.
///
/// This is the state-model invariant the whole design rests on: N concurrent
/// activations of one node id fold through a key-by-key reducer, so they must
/// write disjoint keys.
#[tokio::test]
async fn lanes_write_disjoint_slots_and_leave_the_top_level_alone() {
    let outcome = run_guarded(
        &scatter_graph(json!({ "path": "rows" }), json!({})),
        &mock_capabilities(),
        json!({ "rows": [{ "v": 1 }, { "v": 2 }, { "v": 3 }, { "v": 4 }, { "v": 5 }] }),
    )
    .await
    .expect("run");

    let work = &outcome.output["nodes"]["work"];
    let lanes = work["lanes"].as_object().expect("lane slots");
    assert_eq!(lanes.len(), 5, "every lane kept its own slot");

    // Each lane recorded a distinct index, so none overwrote another.
    let mut indices: Vec<u64> = lanes
        .values()
        .filter_map(|slot| slot["index"].as_u64())
        .collect();
    indices.sort_unstable();
    assert_eq!(indices, vec![0, 1, 2, 3, 4]);

    assert!(
        work.get("items").is_none(),
        "a lane activation must never write the node's top-level items slot, got: {work}"
    );
}

/// `lanes: n` chunks the work rather than opening a lane per item, so a wide
/// input can run a bounded number of lanes.
#[tokio::test]
async fn a_lane_count_chunks_the_work() {
    let rows: Vec<Value> = (0..9).map(|i| json!({ "v": i })).collect();
    let outcome = run_guarded(
        &scatter_graph(json!({ "path": "rows", "lanes": 3 }), json!({})),
        &mock_capabilities(),
        json!({ "rows": rows }),
    )
    .await
    .expect("run");

    let lanes = outcome.output["nodes"]["work"]["lanes"]
        .as_object()
        .expect("lane slots");
    assert_eq!(lanes.len(), 3, "nine items ran in three lanes, not nine");
    assert_eq!(
        outcome.output["nodes"]["collect"]["items"]
            .as_array()
            .map(Vec::len),
        Some(9),
        "all nine items still reach the gather"
    );
}

/// A gather releasing on a quorum emits early rather than waiting for lanes it
/// was told it does not need.
#[tokio::test]
async fn a_gather_can_release_on_a_quorum() {
    let outcome = run_guarded(
        &scatter_graph(
            json!({ "path": "rows" }),
            json!({ "release": "quorum", "n": 2 }),
        ),
        &mock_capabilities(),
        json!({ "rows": [{ "v": "a" }, { "v": "b" }, { "v": "c" }] }),
    )
    .await
    .expect("run");

    let arrived = outcome.output["nodes"]["collect"]["arrived"]
        .as_u64()
        .expect("the gather records how many lanes it saw");
    assert!(
        arrived >= 2,
        "a quorum of 2 must not release with fewer, got {arrived}"
    );
}

/// A scatter over an empty input opens no lanes, and the gather still releases
/// rather than waiting forever for arrivals that can never come.
#[tokio::test]
async fn an_empty_scatter_does_not_hang_the_gather() {
    let outcome = run_guarded(
        &scatter_graph(json!({ "path": "rows" }), json!({})),
        &mock_capabilities(),
        json!({ "rows": [] }),
    )
    .await
    .expect("an empty scatter should complete, not hang");

    assert_eq!(
        outcome.output["nodes"]["collect"]["items"]
            .as_array()
            .map(Vec::len),
        Some(0),
        "no lanes means no results, but the run still finishes"
    );
}

/// A lane that leaves the region without passing through the gather is refused.
///
/// This is the guard on the invariant the whole design rests on. The engine
/// propagates a lane envelope to every successor that is not a gather, so an
/// edge out of the region carries the lane somewhere nothing collects it — and
/// the node on the far side, having no gather to converge on, writes its
/// top-level slot as though it were never in a lane. Wrong output, not a
/// failure, which is exactly what must be caught at author time.
#[tokio::test]
async fn a_lane_escaping_the_region_is_refused() {
    let mut graph = scatter_graph(json!({ "path": "rows" }), json!({}));
    graph
        .nodes
        .push(node("escapee", NodeKind::OutputParser, Value::Null));
    graph.edges.push(edge("work", "escapee"));

    let errors = tinyflows::validate::validate_all(&graph);
    assert!(
        errors.iter().any(|e| format!("{e:?}").contains("escapee")),
        "an edge out of the lane region should be refused, got: {errors:?}"
    );
}

/// A scatter with no gather downstream is refused: its lanes would run with
/// nothing to collect them.
#[tokio::test]
async fn a_scatter_without_a_gather_is_refused() {
    let mut graph = scatter_graph(json!({ "path": "rows" }), json!({}));
    graph.nodes.retain(|n| n.id != "collect");
    graph.edges.retain(|e| e.to_node != "collect");

    let errors = tinyflows::validate::validate_all(&graph);
    assert!(
        errors.iter().any(|e| format!("{e:?}").contains("fan")),
        "a scatter with no gather should be refused, got: {errors:?}"
    );
}

/// A gather with no scatter upstream is refused: it would wait on lanes nobody
/// opens until its poll budget ran out.
#[tokio::test]
async fn a_gather_without_a_scatter_is_refused() {
    let graph = WorkflowGraph {
        name: "orphan_gather".to_string(),
        nodes: vec![
            node("t", NodeKind::Trigger, Value::Null),
            node("work", NodeKind::OutputParser, Value::Null),
            node("collect", NodeKind::Gather, json!({ "from": ["work"] })),
        ],
        edges: vec![edge("t", "work"), edge("work", "collect")],
        ..Default::default()
    };

    let errors = tinyflows::validate::validate_all(&graph);
    assert!(
        errors.iter().any(|e| format!("{e:?}").contains("collect")),
        "an orphan gather should be refused, got: {errors:?}"
    );
}

/// The v1 region restrictions, each refused with its own reason rather than
/// producing a subtly wrong run.
#[tokio::test]
async fn the_unsupported_region_members_are_refused() {
    // A nested scatter: lane ids would have to compose, and the inner gather
    // would have to know which level it closes.
    let mut nested = scatter_graph(json!({ "path": "rows" }), json!({}));
    for n in &mut nested.nodes {
        if n.id == "work" {
            n.kind = NodeKind::Scatter;
            n.config = json!({ "path": "inner" });
        }
    }
    assert!(
        tinyflows::validate::validate_all(&nested)
            .iter()
            .any(|e| format!("{e:?}").contains("nested")),
        "a nested scatter should be refused"
    );

    // An approval gate: a resume is addressed by node id, so every lane of one
    // node would share a single approval.
    let mut gated = scatter_graph(json!({ "path": "rows" }), json!({}));
    for n in &mut gated.nodes {
        if n.id == "work" {
            n.config = json!({ "requires_approval": true });
        }
    }
    assert!(
        tinyflows::validate::validate_all(&gated)
            .iter()
            .any(|e| format!("{e:?}").contains("requires_approval")),
        "an approval gate inside a lane should be refused"
    );
}
