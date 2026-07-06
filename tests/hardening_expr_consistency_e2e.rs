#![cfg(feature = "mock")]
//! Hardening probes for expression consistency (audit BUG-9 inconsistent
//! `=`-resolution, BUG-13 hyphenated node ids). Each test asserts the correct
//! behavior; failing ones are marked `#[ignore]`.

use serde_json::{Value, json};
use tinyflows::caps::mock::{
    MockWorkflowResolver, mock_capabilities, mock_capabilities_with_resolver,
};
use tinyflows::compiler::compile;
use tinyflows::engine::run;
use tinyflows::model::{Edge, Node, NodeKind, WorkflowGraph};

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

fn trigger(id: &str) -> Node {
    node(id, NodeKind::Trigger, Value::Null)
}

fn edge(from_node: &str, to_node: &str) -> Edge {
    Edge {
        from_node: from_node.to_string(),
        from_port: "main".to_string(),
        to_node: to_node.to_string(),
        to_port: "main".to_string(),
    }
}

/// BUG-9 — `sub_workflow` must resolve a `=`-expression in `workflow_id` the
/// same way every other integration node resolves its config. Here the id comes
/// from the trigger payload (`=item.wid` -> `"child-1"`). The correct behavior
/// resolves the child and runs it; treating `"=item.wid"` as a literal id makes
/// the resolver miss and fails the run.
#[tokio::test]
#[ignore = "BUG-9: sub_workflow reads `workflow_id` raw and skips `=`-resolution, so `=item.wid` is passed to the resolver as a literal id, misses, and errors the run (other integration nodes resolve `=`)"]
async fn bug9_sub_workflow_resolves_expression_in_workflow_id() {
    let child = WorkflowGraph {
        name: "child".to_string(),
        nodes: vec![trigger("c_start")],
        ..Default::default()
    };
    let caps =
        mock_capabilities_with_resolver(MockWorkflowResolver::default().with("child-1", child));

    let parent = WorkflowGraph {
        name: "parent".to_string(),
        nodes: vec![
            trigger("start"),
            node(
                "sub",
                NodeKind::SubWorkflow,
                json!({ "workflow_id": "=item.wid" }),
            ),
        ],
        edges: vec![edge("start", "sub")],
        ..Default::default()
    };

    let compiled = compile(&parent).expect("compile parent");
    // If `=item.wid` resolves, the resolver finds `child-1` and the run
    // completes with the child's run state. If it is treated as a literal id,
    // the resolver misses and `run` errors.
    let outcome = run(&compiled, json!({ "wid": "child-1" }), &caps)
        .await
        .expect("run parent");

    let child_state = &outcome.output["nodes"]["sub"]["items"][0]["json"];
    assert!(
        !child_state["run"]["trigger"].is_null(),
        "BUG-9: sub_workflow should resolve `=item.wid` to `child-1` and run the child; observed: \
         `workflow_id` is read raw (literal `\"=item.wid\"`) and the resolver misses"
    );
}

/// BUG-13 — a hyphenated node id must be addressable in a cross-node
/// `=nodes.<id>...` expression. `=nodes.my-node.item.val` should resolve to the
/// upstream value; the hyphen must not be parsed as jq subtraction (-> null).
#[tokio::test]
#[ignore = "BUG-13: `=nodes.my-node.item.val` fails the identifier-only dotted fast-path (hyphen) and falls to jq, which parses `my-node` as subtraction -> null instead of the upstream value"]
async fn bug13_hyphenated_node_id_is_addressable() {
    let graph = WorkflowGraph {
        name: "bug13".to_string(),
        nodes: vec![
            trigger("start"),
            node(
                "my-node",
                NodeKind::Transform,
                json!({ "set": { "val": "hi" } }),
            ),
            node(
                "sink",
                NodeKind::Transform,
                json!({ "set": { "got": "=nodes.my-node.item.val" } }),
            ),
        ],
        edges: vec![edge("start", "my-node"), edge("my-node", "sink")],
        ..Default::default()
    };

    let compiled = compile(&graph).expect("compile");
    let outcome = run(&compiled, json!({}), &mock_capabilities())
        .await
        .expect("run");

    assert_eq!(
        outcome.output["nodes"]["sink"]["items"][0]["json"]["got"], "hi",
        "BUG-13: `=nodes.my-node.item.val` should resolve to `hi`; observed: the hyphen defeats \
         the dotted fast-path and jq parses `my-node` as subtraction -> null"
    );
}

/// Positive regression guard: an identifier-safe node id resolves via
/// `=nodes.<id>.item.<field>` (the common cross-node reference).
#[tokio::test]
async fn good_node_id_cross_ref_resolves() {
    let graph = WorkflowGraph {
        name: "good_ref".to_string(),
        nodes: vec![
            trigger("start"),
            node(
                "source",
                NodeKind::Transform,
                json!({ "set": { "val": "hi" } }),
            ),
            node(
                "sink",
                NodeKind::Transform,
                json!({ "set": { "got": "=nodes.source.item.val" } }),
            ),
        ],
        edges: vec![edge("start", "source"), edge("source", "sink")],
        ..Default::default()
    };

    let compiled = compile(&graph).expect("compile");
    let outcome = run(&compiled, json!({}), &mock_capabilities())
        .await
        .expect("run");

    assert_eq!(
        outcome.output["nodes"]["sink"]["items"][0]["json"]["got"], "hi",
        "an identifier-safe id must resolve via the dotted fast-path"
    );
}
