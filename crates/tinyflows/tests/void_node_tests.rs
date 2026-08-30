#![cfg(feature = "mock")]
//! End-to-end tests for the `void` node kind — the terminal sink.
//!
//! The claim under test is that a void ends its branch *without* costing
//! anything anywhere else: the sibling arm of a fan-out still produces its
//! output, a merge downstream of the other arm still fires, and a loop whose
//! body has a void side branch still iterates to its bound. Those are the
//! failure modes worth guarding, because all three would show up as a hang or a
//! silently missing result rather than as an error.
//!
//! Every run is wrapped in a timeout: a void that somehow stranded a barrier
//! would hang rather than fail, and a hung test takes the suite with it.
//!
//! Gated behind the `mock` cargo feature so plain `cargo test` skips it.

use std::time::Duration;

use serde_json::{Value, json};

use tinyflows::caps::mock::mock_capabilities;
use tinyflows::compiler::compile;
use tinyflows::engine::run;
use tinyflows::model::{Edge, Node, NodeKind, WorkflowGraph};

/// How long any single run in this file may take before it is called a hang.
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

fn port_edge(from: &str, from_port: &str, to: &str) -> Edge {
    Edge {
        from_node: from.to_string(),
        from_port: from_port.to_string(),
        to_node: to.to_string(),
        to_port: "main".to_string(),
    }
}

async fn run_graph(graph: &WorkflowGraph, input: Value) -> Value {
    let compiled = compile(graph).expect("compile");
    let outcome = tokio::time::timeout(GUARD, run(&compiled, input, &mock_capabilities()))
        .await
        .expect("run should not hang")
        .expect("run should succeed");
    assert!(
        outcome.pending_approvals.is_empty(),
        "no approvals expected: {:?}",
        outcome.pending_approvals
    );
    outcome.output
}

#[tokio::test]
async fn fan_out_arm_into_void_does_not_block_the_other_arm() {
    // t -> fan -> {sink(void), keep}. The void arm must neither hold up `keep`
    // nor stop the run from completing.
    let graph = WorkflowGraph {
        name: "void_fan_out".to_string(),
        nodes: vec![
            node("t", NodeKind::Trigger, Value::Null),
            node("fan", NodeKind::Transform, json!({ "set": { "n": 1 } })),
            node(
                "keep",
                NodeKind::Transform,
                json!({ "set": { "tag": "kept" } }),
            ),
            node("sink", NodeKind::Void, Value::Null),
        ],
        edges: vec![edge("t", "fan"), edge("fan", "keep"), edge("fan", "sink")],
        ..Default::default()
    };

    let out = run_graph(&graph, json!({ "seed": 1 })).await;

    assert_eq!(out["nodes"]["keep"]["items"][0]["json"]["tag"], "kept");
    assert_eq!(out["nodes"]["sink"]["items"], json!([]));
    assert_eq!(out["nodes"]["sink"]["discarded"], 1);
    assert!(
        out["nodes"]["sink"]["port"].is_null(),
        "a void routes on no port"
    );
}

#[tokio::test]
async fn a_void_that_never_runs_leaves_no_slot_at_all() {
    // The distinction the `discarded` counter exists to preserve: an untaken
    // branch's void has no slot, which is what separates it from a void that
    // ran and dropped nothing.
    let graph = WorkflowGraph {
        name: "void_untaken".to_string(),
        nodes: vec![
            node("t", NodeKind::Trigger, Value::Null),
            node("check", NodeKind::Condition, json!({ "field": "=false" })),
            node(
                "taken",
                NodeKind::Transform,
                json!({ "set": { "tag": "yes" } }),
            ),
            node("sink", NodeKind::Void, Value::Null),
        ],
        edges: vec![
            edge("t", "check"),
            port_edge("check", "true", "sink"),
            port_edge("check", "false", "taken"),
        ],
        ..Default::default()
    };

    let out = run_graph(&graph, json!({ "seed": 1 })).await;

    assert_eq!(out["nodes"]["taken"]["items"][0]["json"]["tag"], "yes");
    assert!(
        out["nodes"]["sink"].is_null(),
        "the void on the untaken branch never activated, so it has no slot"
    );
}

#[tokio::test]
async fn void_arm_beside_a_merge_does_not_strand_the_barrier() {
    // A void has no outgoing edge, so it can never be in anyone's `waiting`
    // set. This pins that: `m` must still fire on both real predecessors even
    // though a third branch off the same fan-out dead-ends in a void.
    let graph = WorkflowGraph {
        name: "void_merge".to_string(),
        nodes: vec![
            node("t", NodeKind::Trigger, Value::Null),
            node("fan", NodeKind::Transform, json!({ "set": { "n": 1 } })),
            node("a", NodeKind::Transform, json!({ "set": { "arm": "a" } })),
            node("b", NodeKind::Transform, json!({ "set": { "arm": "b" } })),
            node("sink", NodeKind::Void, Value::Null),
            node("m", NodeKind::Merge, Value::Null),
        ],
        edges: vec![
            edge("t", "fan"),
            edge("fan", "a"),
            edge("fan", "b"),
            edge("fan", "sink"),
            edge("a", "m"),
            edge("b", "m"),
        ],
        ..Default::default()
    };

    let out = run_graph(&graph, json!({ "seed": 1 })).await;

    let merged = out["nodes"]["m"]["items"]
        .as_array()
        .expect("the merge should have released");
    let arms: Vec<&str> = merged
        .iter()
        .filter_map(|i| i["json"]["arm"].as_str())
        .collect();
    assert!(
        arms.contains(&"a") && arms.contains(&"b"),
        "the merge must see both real predecessors, got {arms:?}"
    );
    assert_eq!(out["nodes"]["sink"]["discarded"], 1);
}

#[tokio::test]
async fn loop_body_with_a_void_side_branch_runs_every_iteration() {
    // The motivating case: a fire-and-forget side effect hanging off a loop
    // body must not gate the loop. `work` closes the back-edge; `notify -> sink`
    // is the detached arm. If the void participated in re-entry at all, this
    // either hangs (caught by GUARD) or stops short of its iteration bound.
    let graph = WorkflowGraph {
        name: "void_loop".to_string(),
        nodes: vec![
            node(
                "t",
                NodeKind::Trigger,
                json!({ "recursion_limit": 200, "max_node_visits": 200 }),
            ),
            node(
                "head",
                NodeKind::Loop,
                json!({ "max_iterations": 3, "on_exceeded": "continue" }),
            ),
            node(
                "work",
                NodeKind::Transform,
                json!({ "set": { "worked": true } }),
            ),
            node(
                "notify",
                NodeKind::Transform,
                json!({ "set": { "notified": true } }),
            ),
            node("sink", NodeKind::Void, Value::Null),
            node(
                "report",
                NodeKind::Transform,
                json!({ "set": { "done": true } }),
            ),
        ],
        edges: vec![
            edge("t", "head"),
            port_edge("head", "body", "work"),
            port_edge("head", "body", "notify"),
            edge("notify", "sink"),
            edge("work", "head"),
            port_edge("head", "done", "report"),
        ],
        ..Default::default()
    };

    let out = run_graph(&graph, json!({ "seed": 1 })).await;

    assert_eq!(
        out["nodes"]["head"]["iteration"], 3,
        "the loop must reach its bound with the void arm attached"
    );
    assert_eq!(out["nodes"]["report"]["items"][0]["json"]["done"], true);
    assert!(
        !out["nodes"]["sink"].is_null(),
        "the side branch must actually have run"
    );
    assert_eq!(
        out["nodes"]["sink"]["items"],
        json!([]),
        "and it must still have emitted nothing"
    );
}

#[tokio::test]
async fn spawn_into_void_completes_without_a_gate() {
    // `spawn -> void` is the explicit spelling of a ticket nothing will collect.
    // It must behave exactly like leaving the spawn unwired: the run completes,
    // and nothing waits on the task.
    let graph = WorkflowGraph {
        name: "void_spawn".to_string(),
        nodes: vec![
            node("t", NodeKind::Trigger, Value::Null),
            node(
                "kick",
                NodeKind::Spawn,
                json!({ "target": "tool", "slug": "demo" }),
            ),
            node("sink", NodeKind::Void, Value::Null),
        ],
        edges: vec![edge("t", "kick"), edge("kick", "sink")],
        ..Default::default()
    };

    let out = run_graph(&graph, json!({ "seed": 1 })).await;

    let tickets = out["nodes"]["kick"]["items"]
        .as_array()
        .expect("spawn should emit a ticket");
    assert_eq!(tickets.len(), 1, "one ticket per started task");
    assert_eq!(out["nodes"]["sink"]["discarded"], 1);
}

#[tokio::test]
async fn scatter_lane_with_a_void_side_branch_gathers_all_lanes() {
    // A void is the one dead end a lane may have. Every lane must still reach
    // the gather, and the void's per-lane slot must land under `lanes` rather
    // than at the top level.
    let graph = WorkflowGraph {
        name: "void_scatter".to_string(),
        nodes: vec![
            node(
                "t",
                NodeKind::Trigger,
                json!({ "recursion_limit": 400, "max_node_visits": 300 }),
            ),
            node("fan", NodeKind::Scatter, json!({ "path": "rows" })),
            node(
                "work",
                NodeKind::Transform,
                json!({ "set": { "seen": "=item.v" } }),
            ),
            node(
                "notify",
                NodeKind::Transform,
                json!({ "set": { "notified": true } }),
            ),
            node("sink", NodeKind::Void, Value::Null),
            node(
                "collect",
                NodeKind::Gather,
                json!({ "from": ["work"], "poll_interval_ms": 1 }),
            ),
        ],
        edges: vec![
            edge("t", "fan"),
            edge("fan", "work"),
            edge("work", "collect"),
            edge("work", "notify"),
            edge("notify", "sink"),
        ],
        ..Default::default()
    };

    let out = run_graph(
        &graph,
        json!({ "rows": [{ "v": 1 }, { "v": 2 }, { "v": 3 }] }),
    )
    .await;

    let gathered = out["nodes"]["collect"]["items"]
        .as_array()
        .expect("the gather should have released");
    assert_eq!(gathered.len(), 3, "every lane must still be collected");

    let lanes = out["nodes"]["sink"]["lanes"]
        .as_object()
        .expect("a lane activation writes under `lanes`, not the top-level slot");
    assert_eq!(lanes.len(), 3, "the void ran once per lane");
    for (lane, slot) in lanes {
        assert_eq!(slot["items"], json!([]), "lane {lane} emitted nothing");
    }
}
