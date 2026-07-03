#![cfg(feature = "mock")]
//! End-to-end data-flow tests focused on **item pairing** and **multi-item
//! propagation** through the engine.
//!
//! These exercise the item-based currency that flows between nodes (see
//! `src/data.rs`): every element on a connection is an [`Item`] carrying a `json`
//! payload, an optional `binary` attachment, and an optional `paired_item` index
//! that best-effort correlates an output back to the input it derived from.
//!
//! The assertions below encode the *actual* pairing behavior of the current
//! executors, verified against their sources:
//!
//! * `split_out` (`src/nodes/control_flow/split_out.rs`) pairs every fanned-out
//!   element with the index of the **source input item** it came from. With a
//!   single trigger item (index 0), all fanned elements are therefore paired to
//!   `Some(0)` — the pairing is the *source* index, not the element's position.
//! * `transform` (`src/nodes/control_flow/transform.rs`) re-pairs each output to
//!   the **positional index of the item within its own input batch**, discarding
//!   any upstream `paired_item`. So a 3-item batch becomes paired `0, 1, 2`.
//! * `merge` and `output_parser` are identity passthroughs: they preserve each
//!   item (json + pairing) verbatim.
//!
//! Output layout, as produced by `engine::run`:
//! `outcome.output["nodes"]["<id>"]["items"][k]` — each item serializes its
//! `"json"` and, when set, its `"paired_item"` (the `binary` and `paired_item`
//! fields are skipped when `None`).
//!
//! Gated behind the `mock` feature, so `cargo test` (no features) skips this file
//! while `cargo test --all-features` runs it.

use serde_json::{Value, json};
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

/// Borrows the serialized items array a node contributed to the run state.
fn items<'a>(out: &'a Value, node_id: &str) -> &'a Vec<Value> {
    out["nodes"][node_id]["items"]
        .as_array()
        .unwrap_or_else(|| panic!("node `{node_id}` should have produced an items array"))
}

/// Test 1 — `split_out` pairs each fanned element with its **source input index**.
///
/// A single trigger item (index 0) holding a 3-element array fans out into three
/// items, each paired to `Some(0)` — the index of the one input they all came
/// from, per `split_out.rs`'s `Item::new(element).paired_with(index)`.
#[tokio::test]
async fn split_out_sets_source_index_pairing() {
    let graph = WorkflowGraph {
        name: "split_pairing".to_string(),
        nodes: vec![
            trigger("start"),
            node("split", NodeKind::SplitOut, json!({ "path": "xs" })),
        ],
        edges: vec![edge("start", "main", "split")],
        ..Default::default()
    };

    let compiled = compile(&graph).expect("compile");
    let outcome = run(
        &compiled,
        json!({ "xs": [ { "v": 1 }, { "v": 2 }, { "v": 3 } ] }),
        &mock_capabilities(),
    )
    .await
    .expect("run");
    let out = &outcome.output;

    let split = items(out, "split");
    assert_eq!(
        split.len(),
        3,
        "split_out fans the 3-element array into 3 items"
    );

    for (k, expected_v) in [1, 2, 3].into_iter().enumerate() {
        assert_eq!(split[k]["json"], json!({ "v": expected_v }));
        // Every element pairs to source input index 0 (there was one input item).
        assert_eq!(
            split[k]["paired_item"],
            json!(0),
            "split_out pairs every element to its source input index (0), not its own position",
        );
    }
}

/// Test 2 — pairing propagates through a `split_out -> transform -> merge` chain,
/// and the item count is conserved end to end.
///
/// * `split_out` produces 3 items paired `0, 0, 0` (all from source input 0).
/// * `transform` re-pairs to positional indices `0, 1, 2` while applying `set`
///   to each item (`transform.rs` uses `paired_with(index)` over its own batch).
/// * `merge` concatenates/passes through all 3, preserving json + pairing.
/// * The final item count equals the input array length (3).
#[tokio::test]
async fn pairing_propagates_through_split_transform_merge() {
    let graph = WorkflowGraph {
        name: "pairing_chain".to_string(),
        nodes: vec![
            trigger("start"),
            node("split", NodeKind::SplitOut, json!({ "path": "xs" })),
            node(
                "shape",
                NodeKind::Transform,
                json!({ "set": { "seen": true } }),
            ),
            node("collect", NodeKind::Merge, Value::Null),
        ],
        edges: vec![
            edge("start", "main", "split"),
            edge("split", "main", "shape"),
            edge("shape", "main", "collect"),
        ],
        ..Default::default()
    };

    let input_len = 3;
    let compiled = compile(&graph).expect("compile");
    let outcome = run(
        &compiled,
        json!({ "xs": [ { "v": 1 }, { "v": 2 }, { "v": 3 } ] }),
        &mock_capabilities(),
    )
    .await
    .expect("run");
    let out = &outcome.output;

    // Stage 1: split fans out to 3, all paired to source input 0.
    let split = items(out, "split");
    assert_eq!(split.len(), input_len);
    for item in split {
        assert_eq!(item["paired_item"], json!(0));
    }

    // Stage 2: transform preserves the count, applies `set` per item, and re-pairs
    // to its own positional indices 0, 1, 2 (discarding the upstream 0, 0, 0).
    let shape = items(out, "shape");
    assert_eq!(
        shape.len(),
        input_len,
        "transform is 1:1 over its input batch"
    );
    for (k, expected_v) in [1, 2, 3].into_iter().enumerate() {
        assert_eq!(
            shape[k]["json"]["v"],
            json!(expected_v),
            "original field kept"
        );
        assert_eq!(
            shape[k]["json"]["seen"],
            json!(true),
            "`set` applied per item"
        );
        assert_eq!(
            shape[k]["paired_item"],
            json!(k),
            "transform re-pairs each output to its positional input index",
        );
    }

    // Stage 3: merge concatenates all items, preserving json + pairing verbatim.
    let collect = items(out, "collect");
    assert_eq!(
        collect.len(),
        input_len,
        "merge concatenates all items; final count == input array length",
    );
    for (k, expected_v) in [1, 2, 3].into_iter().enumerate() {
        assert_eq!(collect[k]["json"]["v"], json!(expected_v));
        assert_eq!(collect[k]["json"]["seen"], json!(true));
        assert_eq!(
            collect[k]["paired_item"],
            json!(k),
            "merge preserves the pairing transform assigned",
        );
    }
}

/// Test 3 — a multi-item `transform` keeps per-item pairing and evaluates its
/// expression against each item independently.
///
/// Fanning `[{v:10},{v:20},{v:30}]` through `split_out` gives `transform` a
/// 3-item batch; `set.copy_v = =item.v` copies each item's own `v`, proving the
/// transform runs per item, and each output is paired to its positional index.
#[tokio::test]
async fn multi_item_transform_keeps_pairing_and_applies_per_item() {
    let graph = WorkflowGraph {
        name: "multi_item_transform".to_string(),
        nodes: vec![
            trigger("start"),
            node("split", NodeKind::SplitOut, json!({ "path": "xs" })),
            node(
                "shape",
                NodeKind::Transform,
                json!({ "set": { "copy_v": "=item.v" } }),
            ),
        ],
        edges: vec![
            edge("start", "main", "split"),
            edge("split", "main", "shape"),
        ],
        ..Default::default()
    };

    let compiled = compile(&graph).expect("compile");
    let outcome = run(
        &compiled,
        json!({ "xs": [ { "v": 10 }, { "v": 20 }, { "v": 30 } ] }),
        &mock_capabilities(),
    )
    .await
    .expect("run");
    let out = &outcome.output;

    let shape = items(out, "shape");
    assert_eq!(shape.len(), 3, "transform maps 1:1 over the 3 fanned items");
    for (k, expected_v) in [10, 20, 30].into_iter().enumerate() {
        // Per-item evaluation: each output's copy_v equals *that* item's v.
        assert_eq!(shape[k]["json"]["v"], json!(expected_v));
        assert_eq!(
            shape[k]["json"]["copy_v"],
            json!(expected_v),
            "the expression is evaluated against each item independently",
        );
        assert_eq!(
            shape[k]["paired_item"],
            json!(k),
            "each output keeps its positional pairing index",
        );
    }
}

// NOTE: `binary` is currently plumbed only at the `Item` level (`src/data.rs`) and
// is exercised solely at the serde layer — no executor or mock capability ever
// produces a non-null `binary` attachment (verified across `src/nodes/**` and
// `src/caps/mock.rs`). This test therefore documents that reality rather than
// inventing behavior: with a json-only seed, items flowing through the engine
// carry no `binary` field (it is `Option::None` and skipped from the wire form).
/// Test 4 — `binary` stays absent end to end when no node emits it.
///
/// Seeding a json-only trigger and flowing through `split_out` and the
/// identity-passthrough `output_parser`, every serialized item omits `binary`.
#[tokio::test]
async fn binary_field_absent_when_no_node_emits_it() {
    let graph = WorkflowGraph {
        name: "binary_absent".to_string(),
        nodes: vec![
            trigger("start"),
            node("split", NodeKind::SplitOut, json!({ "path": "xs" })),
            node("parse", NodeKind::OutputParser, Value::Null),
        ],
        edges: vec![
            edge("start", "main", "split"),
            edge("split", "main", "parse"),
        ],
        ..Default::default()
    };

    let compiled = compile(&graph).expect("compile");
    let outcome = run(
        &compiled,
        json!({ "xs": [ { "v": 1 }, { "v": 2 } ] }),
        &mock_capabilities(),
    )
    .await
    .expect("run");
    let out = &outcome.output;

    for node_id in ["split", "parse"] {
        let stage = items(out, node_id);
        assert!(!stage.is_empty(), "node `{node_id}` produced items");
        for item in stage {
            // `binary` is skipped when `None`, so it is absent from the wire form.
            assert!(
                item.get("binary").is_none(),
                "no executor emits `binary`, so `{node_id}` items omit the field",
            );
        }
    }
}
