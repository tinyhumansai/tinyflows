#![cfg(feature = "mock")]
//! Lane isolation observed write by write, not only in the final state.
//!
//! `fuzz_scatter_tests.rs` checks what a scatter run *ends* with. A lane that
//! wrote its slot twice, or wrote the node's top-level slot and was later
//! overwritten, ends the same way. There is no write hook, so this watches the
//! two public observation points instead:
//!
//! - every super-step boundary's state, from an [`InMemoryCheckpointer`]: a
//!   lane slot goes absent → present once and never changes after, and a
//!   lane-body node's top-level `items` never appears at any boundary;
//! - every activation's `StepPhase::After` frame: each (node, lane) pair
//!   activates exactly once, and no body node activates outside a lane. This
//!   catches two writes inside one step, which a boundary diff cannot see.
//!
//! Only the paths that are supposed to be lane-local are generated: success,
//! `on_error: continue` and `on_error: route`. Cancellation, retry backoff and
//! `Reenter` (a gate or spawn in a lane body) write the top-level slot today
//! and are deliberately out of scope here.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use proptest::prelude::*;
use serde_json::{Value, json};
use tinyflows::caps::ToolInvoker;
use tinyflows::caps::mock::mock_capabilities;
use tinyflows::compiler::{CompiledWorkflow, compile};
use tinyflows::engine::{
    CancellationToken, Checkpointer, InMemoryCheckpointer, run_intercepted, run_with_checkpointer,
};
use tinyflows::interception::{StepAction, StepFrame, StepInterceptor, StepPhase};
use tinyflows::model::{Edge, Node, NodeKind, WorkflowGraph};
use tinyflows::observability::{NoopObserver, RunObserver};

const GUARD: Duration = Duration::from_secs(5);
const THREAD: &str = "lane-isolation";

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

fn edge(from: &str, port: &str, to: &str) -> Edge {
    Edge {
        from_node: from.to_string(),
        from_port: port.to_string(),
        to_node: to.to_string(),
        to_port: "main".to_string(),
    }
}

/// How the lane body ends.
#[derive(Debug, Clone, Copy)]
enum Tail {
    /// Transforms only.
    Plain,
    /// A tool call with `on_error: continue`.
    Continue,
    /// A tool call with `on_error: route` into `ok` / `recover` arms.
    Route,
}

/// trigger → scatter → `work_0..n` transforms → [tail] → gather.
///
/// Returns the graph and the ids of every node that runs inside a lane.
fn graph(body_len: usize, lanes: Option<usize>, tail: Tail) -> (WorkflowGraph, Vec<String>) {
    let mut scatter = json!({ "path": "rows" });
    if let Some(lanes) = lanes {
        scatter["lanes"] = json!(lanes);
    }
    let mut nodes = vec![
        node(
            "trigger",
            NodeKind::Trigger,
            json!({ "recursion_limit": 500, "max_node_visits": 300 }),
        ),
        node("scatter", NodeKind::Scatter, scatter),
    ];
    let mut edges = vec![edge("trigger", "main", "scatter")];
    let mut members = Vec::new();
    let mut previous = "scatter".to_string();
    for index in 0..body_len {
        let id = format!("work_{index}");
        nodes.push(node(
            &id,
            NodeKind::Transform,
            json!({ "set": { format!("stage_{index}"): index } }),
        ));
        edges.push(edge(&previous, "main", &id));
        members.push(id.clone());
        previous = id;
    }
    let call = |policy: &str| {
        node(
            "call",
            NodeKind::ToolCall,
            json!({ "slug": "lane.maybe_fail", "args": "=item", "on_error": policy }),
        )
    };
    let from: Vec<String> = match tail {
        Tail::Plain => vec![previous],
        Tail::Continue => {
            nodes.push(call("continue"));
            edges.push(edge(&previous, "main", "call"));
            members.push("call".to_string());
            vec!["call".to_string()]
        }
        Tail::Route => {
            nodes.push(call("route"));
            nodes.push(node(
                "ok",
                NodeKind::Transform,
                json!({ "set": { "arm": "ok" } }),
            ));
            nodes.push(node(
                "recover",
                NodeKind::Transform,
                json!({ "set": { "arm": "recover" } }),
            ));
            edges.push(edge(&previous, "main", "call"));
            edges.push(edge("call", "main", "ok"));
            edges.push(edge("call", "error", "recover"));
            members.extend(["call", "ok", "recover"].map(String::from));
            vec!["ok".to_string(), "recover".to_string()]
        }
    };
    nodes.push(node(
        "gather",
        NodeKind::Gather,
        json!({ "from": from.clone(), "poll_interval_ms": 1 }),
    ));
    for pred in &from {
        edges.push(edge(pred, "main", "gather"));
    }
    let graph = WorkflowGraph {
        name: "generated_lane_writes".to_string(),
        nodes,
        edges,
        ..Default::default()
    };
    (graph, members)
}

fn expected_lane_count(items: usize, requested: Option<usize>) -> usize {
    if items == 0 {
        return 0;
    }
    let requested = requested.unwrap_or(items).clamp(1, 256);
    if requested >= items {
        items
    } else {
        items.div_ceil(items.div_ceil(requested))
    }
}

/// Fails any call whose input row carries `fail: true`.
struct MaybeFail;

#[async_trait]
impl ToolInvoker for MaybeFail {
    async fn invoke(
        &self,
        _slug: &str,
        args: Value,
        _conn: Option<&str>,
    ) -> tinyflows::error::Result<Value> {
        if args.get("fail").and_then(Value::as_bool) == Some(true) {
            Err(tinyflows::error::EngineError::Capability(
                "generated lane failure".to_string(),
            ))
        } else {
            Ok(args)
        }
    }
}

/// Records `(node id, lane id)` for every `After` frame.
#[derive(Default)]
struct AfterFrames(Mutex<Vec<(String, Option<String>)>>);

#[async_trait]
impl StepInterceptor for AfterFrames {
    async fn intercept(&self, frame: StepFrame<'_>) -> StepAction {
        if frame.phase == StepPhase::After {
            self.0.lock().expect("frames mutex poisoned").push((
                frame.node.id.clone(),
                frame.lane.map(|lane| lane.id.clone()),
            ));
        }
        StepAction::Continue { state_patch: None }
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build runtime")
}

/// The run state at every super-step boundary, in order.
fn boundary_states(compiled: &CompiledWorkflow, input: Value) -> Vec<Value> {
    let mut caps = mock_capabilities();
    caps.tools = Arc::new(MaybeFail);
    runtime().block_on(async {
        let checkpointer = Arc::new(InMemoryCheckpointer::<Value>::new());
        tokio::time::timeout(
            GUARD,
            run_with_checkpointer(compiled, input, &caps, checkpointer.clone(), THREAD),
        )
        .await
        .expect("scatter run hung")
        .expect("scatter run");
        checkpointer
            .get_thread(THREAD)
            .await
            .expect("read checkpoints")
            .into_iter()
            .map(|checkpoint| checkpoint.state)
            .collect()
    })
}

/// Every `After` frame of a second, identical run.
fn after_frames(compiled: &CompiledWorkflow, input: Value) -> Vec<(String, Option<String>)> {
    let mut caps = mock_capabilities();
    caps.tools = Arc::new(MaybeFail);
    let frames = Arc::new(AfterFrames::default());
    runtime().block_on(async {
        tokio::time::timeout(
            GUARD,
            run_intercepted(
                compiled,
                input,
                &caps,
                &(Arc::new(NoopObserver) as Arc<dyn RunObserver>),
                CancellationToken::new(),
                frames.clone(),
            ),
        )
        .await
        .expect("intercepted scatter run hung")
        .expect("intercepted scatter run");
    });
    frames.0.lock().expect("frames mutex poisoned").clone()
}

fn lane_map<'a>(state: &'a Value, id: &str) -> Option<&'a serde_json::Map<String, Value>> {
    state["nodes"][id].get("lanes").and_then(Value::as_object)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

    #[test]
    fn every_lane_slot_is_written_once_and_never_the_top_level(
        rows in prop::collection::vec((any::<i16>(), any::<bool>()), 1..33),
        body_len in 1usize..4,
        requested in prop::option::of(1usize..17),
        tail in prop_oneof![Just(Tail::Plain), Just(Tail::Continue), Just(Tail::Route)],
    ) {
        let (graph, members) = graph(body_len, requested, tail);
        let compiled = compile(&graph).expect("generated graph compiles");
        let rows: Vec<Value> = rows
            .iter()
            .map(|(value, fail)| json!({ "value": value, "fail": fail }))
            .collect();
        let expected = expected_lane_count(rows.len(), requested);
        let input = json!({ "rows": rows });

        // Boundary by boundary: a lane slot appears once and is then frozen;
        // the top-level slot never appears at all.
        let states = boundary_states(&compiled, input.clone());
        prop_assert!(!states.is_empty(), "the checkpointer recorded no boundaries");
        let mut first_seen: BTreeMap<(String, String), Value> = BTreeMap::new();
        for (boundary, state) in states.iter().enumerate() {
            for id in &members {
                prop_assert!(
                    state["nodes"][id].get("items").is_none(),
                    "boundary {boundary}: a lane activation wrote {id}'s top-level slot: {}",
                    state["nodes"][id]
                );
                for (lane, slot) in lane_map(state, id).into_iter().flatten() {
                    let key = (id.clone(), lane.clone());
                    match first_seen.get(&key) {
                        Some(first) => prop_assert_eq!(
                            first, slot,
                            "boundary {}: lane slot {}.{} was rewritten", boundary, id, lane
                        ),
                        None => {
                            first_seen.insert(key, slot.clone());
                        }
                    }
                }
            }
        }

        let last = states.last().expect("at least one boundary");
        let count = |id: &str| lane_map(last, id).map_or(0, serde_json::Map::len);
        for id in members.iter().filter(|id| !matches!(id.as_str(), "ok" | "recover")) {
            prop_assert_eq!(count(id), expected, "wrong lane count at {}", id);
        }
        if matches!(tail, Tail::Route) {
            prop_assert_eq!(count("ok") + count("recover"), expected, "a lane was lost or doubled across the route arms");
            if let (Some(ok), Some(recover)) = (lane_map(last, "ok"), lane_map(last, "recover")) {
                prop_assert!(
                    ok.keys().all(|lane| !recover.contains_key(lane)),
                    "one lane took both route arms"
                );
            }
        }

        // Activation by activation: exactly one `After` frame per (node, lane),
        // and never an unlaned activation of a lane-body node.
        let frames = after_frames(&compiled, input);
        let mut per_lane: BTreeMap<(String, String), usize> = BTreeMap::new();
        for (id, lane) in frames.into_iter().filter(|(id, _)| members.contains(id)) {
            prop_assert!(lane.is_some(), "{} activated outside any lane", id);
            if let Some(lane) = lane {
                *per_lane.entry((id, lane)).or_default() += 1;
            }
        }
        for ((id, lane), activations) in &per_lane {
            prop_assert_eq!(*activations, 1, "{}.{} activated more than once", id, lane);
        }
        let first_seen_keys: Vec<_> = first_seen.keys().cloned().collect();
        let frame_keys: Vec<_> = per_lane.keys().cloned().collect();
        prop_assert_eq!(first_seen_keys, frame_keys, "lane slots and lane activations disagree");
    }
}
