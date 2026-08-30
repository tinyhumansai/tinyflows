#![cfg(feature = "mock")]
//! End-to-end integration tests for parallel fan-out and the `merge` fan-in.
//!
//! When a node has more than one outgoing edge that all share a single output
//! port, the engine treats it as a **parallel fan-out**: every successor runs
//! concurrently in the same superstep. A `merge` node with multiple predecessors
//! is lowered with waiting edges, so it runs only once *every* predecessor has
//! finished (the merge barrier) and its input is the concatenation of all
//! predecessors' items.
//!
//! These tests assert (a) a 3-way fan-out joined by a merge yields 3 items at the
//! merge, and (b) in a diamond each branch contributes its own distinctly-marked
//! item to the merge.
//!
//! Gated behind the `mock` cargo feature.

use serde_json::{Value, json};
use std::collections::HashSet;
use tinyflows::caps::mock::mock_capabilities;
use tinyflows::compiler::compile;
use tinyflows::engine::run;
use tinyflows::model::{Edge, Node, NodeKind, TriggerKind, WorkflowGraph};

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

/// Builds a manual trigger node.
fn trigger(id: &str) -> Node {
    node(
        id,
        NodeKind::Trigger,
        json!({ "kind": TriggerKind::Manual }),
    )
}

/// Builds an edge from `from_node`'s `from_port` into `to_node`'s `"main"` port.
fn edge(from_node: &str, from_port: &str, to_node: &str) -> Edge {
    Edge {
        from_node: from_node.to_string(),
        from_port: from_port.to_string(),
        to_node: to_node.to_string(),
        to_port: "main".to_string(),
    }
}

#[tokio::test]
async fn fan_out_to_three_successors_joins_at_merge() {
    // trigger fans out (three `main`-port edges) to three independent http nodes,
    // each producing exactly one item; a merge concatenates all three.
    let graph = WorkflowGraph {
        name: "fan_out_merge".to_string(),
        nodes: vec![
            trigger("start"),
            node(
                "a",
                NodeKind::HttpRequest,
                json!({ "method": "GET", "url": "https://svc/a" }),
            ),
            node(
                "b",
                NodeKind::HttpRequest,
                json!({ "method": "GET", "url": "https://svc/b" }),
            ),
            node(
                "c",
                NodeKind::HttpRequest,
                json!({ "method": "GET", "url": "https://svc/c" }),
            ),
            node("join", NodeKind::Merge, Value::Null),
        ],
        edges: vec![
            edge("start", "main", "a"),
            edge("start", "main", "b"),
            edge("start", "main", "c"),
            edge("a", "main", "join"),
            edge("b", "main", "join"),
            edge("c", "main", "join"),
        ],
        ..Default::default()
    };

    let compiled = compile(&graph).expect("compile");
    let outcome = run(&compiled, json!({ "seed": 1 }), &mock_capabilities())
        .await
        .expect("run");
    let out = &outcome.output;

    // All three concurrent successors ran.
    for id in ["a", "b", "c"] {
        assert_eq!(
            out["nodes"][id]["items"][0]["json"]["json"]["status"], 200,
            "fan-out branch {id} should have executed"
        );
    }

    // The merge barrier waited for all three, then concatenated their items.
    let merged = out["nodes"]["join"]["items"]
        .as_array()
        .expect("merge produced an items array");
    assert_eq!(
        merged.len(),
        3,
        "merge should combine one item from each of the three predecessors"
    );

    // The three merged items are the three distinct upstream URLs.
    let urls: HashSet<String> = merged
        .iter()
        .map(|item| {
            item["json"]["json"]["request"]["url"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(
        urls,
        HashSet::from([
            "https://svc/a".to_string(),
            "https://svc/b".to_string(),
            "https://svc/c".to_string(),
        ])
    );
}

#[tokio::test]
async fn diamond_every_branch_contributes_to_merge() {
    // A diamond with a non-trigger apex: trigger -> apex(code) fans out to three
    // transforms, each stamping a distinct `branch` marker, then a merge joins
    // them. Every branch must contribute exactly one item to the merge.
    let graph = WorkflowGraph {
        name: "diamond".to_string(),
        nodes: vec![
            trigger("start"),
            node(
                "apex",
                NodeKind::Code,
                json!({ "language": "python", "source": "passthrough(input)" }),
            ),
            node(
                "branch_a",
                NodeKind::Transform,
                json!({ "set": { "branch": "a" } }),
            ),
            node(
                "branch_b",
                NodeKind::Transform,
                json!({ "set": { "branch": "b" } }),
            ),
            node(
                "branch_c",
                NodeKind::Transform,
                json!({ "set": { "branch": "c" } }),
            ),
            node("join", NodeKind::Merge, Value::Null),
        ],
        edges: vec![
            edge("start", "main", "apex"),
            edge("apex", "main", "branch_a"),
            edge("apex", "main", "branch_b"),
            edge("apex", "main", "branch_c"),
            edge("branch_a", "main", "join"),
            edge("branch_b", "main", "join"),
            edge("branch_c", "main", "join"),
        ],
        ..Default::default()
    };

    let compiled = compile(&graph).expect("compile");
    let outcome = run(&compiled, json!({ "seed": 7 }), &mock_capabilities())
        .await
        .expect("run");
    let out = &outcome.output;

    let merged = out["nodes"]["join"]["items"]
        .as_array()
        .expect("merge produced an items array");
    assert_eq!(
        merged.len(),
        3,
        "each of the three branches contributes one item"
    );

    // Every branch's distinct marker is present exactly once.
    let branches: HashSet<String> = merged
        .iter()
        .map(|item| item["json"]["branch"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        branches,
        HashSet::from(["a".to_string(), "b".to_string(), "c".to_string()]),
        "all three branches must be represented in the merge"
    );

    // Each merged item still carries the apex code node's `result` payload,
    // proving data flowed apex -> branch -> merge.
    for item in merged {
        assert!(
            !item["json"]["result"].is_null(),
            "merged item should retain the apex code node's result"
        );
    }
}

/// `trigger.config.max_concurrency` bounds how many branches of a super-step
/// run at once, without changing what the run computes.
///
/// Eight branches fan out from one node. With the dial set to 2, at most two may
/// ever be in flight — measured as observed overlap, since a bound that silently
/// failed to apply would produce the identical final state.
///
/// This is admission control, not backpressure: the engine cannot block a
/// branch mid-step, only decide how many are allowed to start.
#[tokio::test]
async fn max_concurrency_bounds_how_many_branches_run_at_once() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A tool that reports the peak number of concurrent invocations it saw.
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
            // Yield enough that unbounded branches would visibly overlap.
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            Ok(json!({ "ok": true }))
        }
    }

    async fn peak_overlap(max_concurrency: Option<u64>) -> usize {
        let probe = Arc::new(OverlapProbe {
            in_flight: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        });
        let mut caps = mock_capabilities();
        caps.tools = probe.clone();

        let mut trigger_config = json!({});
        if let Some(limit) = max_concurrency {
            trigger_config["max_concurrency"] = json!(limit);
        }

        let branches: Vec<String> = (0..8).map(|i| format!("b{i}")).collect();
        let mut nodes = vec![
            node("t", NodeKind::Trigger, trigger_config),
            node("apex", NodeKind::OutputParser, Value::Null),
        ];
        let mut edges = vec![edge("t", "main", "apex")];
        for id in &branches {
            nodes.push(node(id, NodeKind::ToolCall, json!({ "slug": "probe.run" })));
            edges.push(edge("apex", "main", id));
        }

        let graph = WorkflowGraph {
            name: "concurrency_dial".to_string(),
            nodes,
            edges,
            ..Default::default()
        };
        let compiled = compile(&graph).expect("compile");
        run(&compiled, json!({}), &caps).await.expect("run");
        probe.peak.load(Ordering::SeqCst)
    }

    let bounded = peak_overlap(Some(2)).await;
    assert!(
        bounded <= 2,
        "max_concurrency: 2 must bound in-flight branches, observed peak {bounded}"
    );

    // Without the dial the same graph genuinely overlaps more, which is what
    // makes the assertion above meaningful rather than vacuously true.
    let unbounded = peak_overlap(None).await;
    assert!(
        unbounded > 2,
        "the same graph should overlap more without the dial, observed peak \
         {unbounded} — if this is low the probe is not actually concurrent and \
         the bounded assertion proves nothing"
    );
}
