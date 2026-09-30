use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::*;

fn graph(value: Value) -> WorkflowGraph {
    serde_json::from_value(value).expect("graph parses")
}

fn ordered_graph() -> WorkflowGraph {
    graph(json!({
        "name": "order-test",
        "nodes": [
            { "id": "t", "kind": "trigger", "name": "Trigger" },
            {
                "id": "n",
                "kind": "output_parser",
                "name": "N",
                "config": { "a": 1, "b": 2, "nested": { "x": 1, "y": 2 } }
            }
        ],
        "edges": [ { "from_node": "t", "to_node": "n" } ]
    }))
}

/// The pre-extraction implementation, verbatim, kept as an oracle: a persisted
/// pin was produced by exactly this code, so the moved function must agree with
/// it on every input.
fn legacy_reference(graph: &WorkflowGraph, require_approval: bool) -> Option<String> {
    fn canon(value: &Value) -> Value {
        match value {
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                let mut sorted = serde_json::Map::new();
                for key in keys {
                    sorted.insert(key.clone(), canon(&map[key]));
                }
                Value::Object(sorted)
            }
            Value::Array(items) => Value::Array(items.iter().map(canon).collect()),
            other => other.clone(),
        }
    }
    let raw = serde_json::to_value(graph).ok()?;
    let raw = json!({ "graph": raw, "require_approval": require_approval });
    let serialized = serde_json::to_string(&canon(&raw)).ok()?;
    Some(hex::encode(Sha256::digest(serialized.as_bytes())))
}

/// Persisted run pins depend on this exact digest. If this test fails, a
/// parked run created before the change would be refused on resume: do not
/// "fix" the constant, fix the hash.
#[test]
fn hash_matches_a_fixed_vector() {
    let g = ordered_graph();
    assert_eq!(
        compute_graph_hash(&g, false).as_deref(),
        Some("33ff2f572641fba1b81c6726161314f8a9e725a6b8c33a0a119cbe0564b57ad2")
    );
    assert_eq!(
        compute_graph_hash(&g, true).as_deref(),
        Some("a13666dfbcff29a5509b0ee4d0bc50199b47eb11cb32f5ba2b00c7ae626cd179")
    );
}

#[test]
fn hash_agrees_with_the_pre_extraction_implementation() {
    let mut extended = json!({
        "name": "wide",
        "nodes": [
            { "id": "t", "kind": "trigger", "name": "Trigger" },
            { "id": "n", "kind": "output_parser", "name": "N",
              "config": { "z": [3, 2, 1], "é": "ünï", "f": 1.5, "nested": { "b": null, "a": true } } }
        ],
        "edges": [ { "from_node": "t", "to_node": "n" } ]
    });
    extended["nodes"][1]["config"]["extra"] = json!({ "y": 1, "x": 2 });
    for g in [
        ordered_graph(),
        graph(extended),
        graph(json!({})),
        WorkflowGraph::default(),
    ] {
        for approval in [false, true] {
            assert_eq!(
                compute_graph_hash(&g, approval),
                legacy_reference(&g, approval)
            );
        }
    }
}

/// The hash covers graph *content*, not incidental JSON object key order.
#[test]
fn hash_is_stable_across_serialization_key_order() {
    let graph_a = ordered_graph();
    let graph_b = graph(json!({
        "name": "order-test",
        "nodes": [
            { "id": "t", "kind": "trigger", "name": "Trigger" },
            {
                "id": "n",
                "kind": "output_parser",
                "name": "N",
                "config": { "nested": { "y": 2, "x": 1 }, "b": 2, "a": 1 }
            }
        ],
        "edges": [ { "from_node": "t", "to_node": "n" } ]
    }));
    assert_eq!(
        compute_graph_hash(&graph_a, false),
        compute_graph_hash(&graph_b, false),
        "the same graph content in a different key order must hash identically"
    );

    let mut changed = json!({
        "name": "order-test",
        "nodes": [
            { "id": "t", "kind": "trigger", "name": "Trigger" },
            { "id": "n", "kind": "output_parser", "name": "N",
              "config": { "a": 1, "b": 2, "nested": { "x": 1, "y": 2 } } }
        ],
        "edges": [ { "from_node": "t", "to_node": "n" } ]
    });
    changed["nodes"][1]["config"]["a"] = json!(999);
    assert_ne!(
        compute_graph_hash(&graph_a, false),
        compute_graph_hash(&graph(changed), false),
        "a genuinely different graph must not collide"
    );
}

/// `require_approval` governs every outbound call in a resumed run and is
/// settable independently of the graph, so flipping it must invalidate a pin.
#[test]
fn hash_covers_require_approval_not_just_the_graph() {
    let g = ordered_graph();
    let gated = compute_graph_hash(&g, true).expect("hashes");
    let ungated = compute_graph_hash(&g, false).expect("hashes");
    assert_ne!(gated, ungated);
    assert_eq!(Some(gated), compute_graph_hash(&g, true));
}

/// Array order is semantic (edge and node order), so reordering changes the pin.
#[test]
fn hash_is_sensitive_to_array_order() {
    let a = graph(json!({
        "nodes": [
            { "id": "a", "kind": "trigger", "name": "A" },
            { "id": "b", "kind": "output_parser", "name": "B" }
        ]
    }));
    let b = graph(json!({
        "nodes": [
            { "id": "b", "kind": "output_parser", "name": "B" },
            { "id": "a", "kind": "trigger", "name": "A" }
        ]
    }));
    assert_ne!(compute_graph_hash(&a, false), compute_graph_hash(&b, false));
}
