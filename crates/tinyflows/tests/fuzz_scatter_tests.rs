#![cfg(feature = "mock")]
//! Property tests for scatter lane isolation and ordered gathering.

use std::time::Duration;

use proptest::prelude::*;
use serde_json::{Value, json};
use tinyflows::caps::mock::mock_capabilities;
use tinyflows::compiler::compile;
use tinyflows::engine::run;
use tinyflows::model::{Edge, Node, NodeKind, WorkflowGraph};

const GUARD: Duration = Duration::from_secs(5);

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

fn graph(body_len: usize, requested_lanes: Option<usize>) -> WorkflowGraph {
    let mut scatter_config = json!({ "path": "rows" });
    if let Some(lanes) = requested_lanes {
        scatter_config["lanes"] = json!(lanes);
    }
    let mut nodes = vec![
        node(
            "trigger",
            NodeKind::Trigger,
            json!({ "recursion_limit": 500, "max_node_visits": 300 }),
        ),
        node("scatter", NodeKind::Scatter, scatter_config),
    ];
    let mut edges = vec![edge("trigger", "scatter")];
    let mut previous = "scatter".to_string();
    for index in 0..body_len {
        let id = format!("work_{index}");
        nodes.push(node(
            &id,
            NodeKind::Transform,
            json!({ "set": { format!("stage_{index}"): index } }),
        ));
        edges.push(edge(&previous, &id));
        previous = id;
    }
    nodes.push(node(
        "gather",
        NodeKind::Gather,
        json!({ "from": [previous.clone()], "poll_interval_ms": 1 }),
    ));
    edges.push(edge(&previous, "gather"));
    WorkflowGraph {
        name: "generated_lane_isolation".to_string(),
        nodes,
        edges,
        ..Default::default()
    }
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

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build runtime")
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 96, ..ProptestConfig::default() })]

    #[test]
    fn every_lane_has_one_disjoint_slot_at_every_body_node(
        values in prop::collection::vec(any::<i16>(), 0..65),
        body_len in 1usize..6,
        requested in prop::option::of(1usize..33),
    ) {
        let graph = graph(body_len, requested);
        let compiled = compile(&graph).expect("generated graph compiles");
        let rows: Vec<Value> = values.iter().map(|value| json!({ "value": value })).collect();
        let outcome = runtime().block_on(async {
            tokio::time::timeout(
                GUARD,
                run(&compiled, json!({ "rows": rows }), &mock_capabilities()),
            )
            .await
            .expect("scatter/gather run hung")
            .expect("generated run")
        });

        let expected = expected_lane_count(values.len(), requested);
        for index in 0..body_len {
            let id = format!("work_{index}");
            let slot = &outcome.output["nodes"][&id];
            // An empty scatter carries one empty, ordinary activation through
            // its body so the downstream gather gets a chance to release.
            // Non-empty scatters must use lane slots exclusively.
            if expected > 0 {
                prop_assert!(
                    slot.get("items").is_none(),
                    "lane activation wrote {id}'s top-level items: {slot}"
                );
            }
            let actual = slot
                .get("lanes")
                .and_then(Value::as_object)
                .map_or(0, serde_json::Map::len);
            prop_assert_eq!(actual, expected, "wrong lane count at {}", id);

            if let Some(lanes) = slot.get("lanes").and_then(Value::as_object) {
                let mut indices: Vec<u64> = lanes
                    .values()
                    .filter_map(|lane| lane.get("index").and_then(Value::as_u64))
                    .collect();
                indices.sort_unstable();
                prop_assert_eq!(indices, (0..expected as u64).collect::<Vec<_>>());
            }
        }

        let gathered = outcome.output["nodes"]["gather"]["items"]
            .as_array()
            .expect("gather items");
        let actual: Vec<i64> = gathered
            .iter()
            .filter_map(|item| item["json"]["value"].as_i64())
            .collect();
        let expected_values: Vec<i64> = values.iter().map(|value| i64::from(*value)).collect();
        prop_assert_eq!(actual, expected_values, "gather changed input order or cardinality");
    }
}
