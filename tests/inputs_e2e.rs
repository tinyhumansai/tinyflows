#![cfg(feature = "mock")]
//! End-to-end tests for declared workflow inputs.
//!
//! A graph declares typed parameters in its top-level `inputs` array; a caller
//! supplies values through [`RunInput`], they are validated before anything
//! executes, and node config reads them as `=inputs.<name>` (see
//! `src/model/inputs.rs` and the `inputs` scope key in `src/nodes/mod.rs`).
//!
//! These tests assert the *whole chain* rather than any one layer: that a
//! supplied value reaches a node's resolved config, that defaults are applied,
//! that a rejected call runs nothing at all, and that a parent forwards values
//! to a `sub_workflow` child.
//!
//! Gated behind the `mock` feature, so plain `cargo test` skips it while
//! `cargo test --features mock` runs it.

use serde_json::{Map, Value, json};
use tinyflows::caps::mock::{
    MockWorkflowResolver, mock_capabilities, mock_capabilities_with_resolver,
};
use tinyflows::compiler::compile;
use tinyflows::engine::{RunInput, run};
use tinyflows::error::EngineError;
use tinyflows::model::{
    Edge, InputType, Node, NodeKind, TriggerKind, WorkflowGraph, WorkflowInput,
};

/// Builds a node with the given id, kind, and config.
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

/// Builds a trigger node with the given firing mode.
fn trigger(id: &str, kind: TriggerKind) -> Node {
    node(id, NodeKind::Trigger, json!({ "kind": kind }))
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

/// Collects `pairs` into the supplied-values map a caller hands [`RunInput`].
fn values(pairs: &[(&str, Value)]) -> Map<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), v.clone()))
        .collect()
}

/// A graph declaring `repo` (required) and `depth` (defaulted), whose single
/// `transform` node copies both into its output via `=inputs.<name>`.
///
/// `transform` is used because the mock capabilities echo it deterministically,
/// so the assertion is on the *resolved* config values, not on any provider's
/// behaviour.
fn parameterized_graph() -> WorkflowGraph {
    WorkflowGraph {
        name: "parameterized".to_string(),
        inputs: vec![
            WorkflowInput::new("repo", InputType::String)
                .required()
                .with_description("Repo to review"),
            WorkflowInput::new("depth", InputType::Number).with_default(json!(3)),
            WorkflowInput::new("note", InputType::String),
        ],
        nodes: vec![
            trigger("t", TriggerKind::Manual),
            node(
                "shape",
                NodeKind::Transform,
                json!({ "set": { "repo": "=inputs.repo", "depth": "=inputs.depth", "note": "=inputs.note" } }),
            ),
        ],
        edges: vec![edge("t", "shape")],
        ..Default::default()
    }
}

#[tokio::test]
async fn supplied_input_reaches_node_config_and_defaults_are_applied() {
    let compiled = compile(&parameterized_graph()).expect("compile");
    let caps = mock_capabilities();

    let outcome = run(
        &compiled,
        RunInput::new(json!({})).with_inputs(values(&[("repo", json!("acme/api"))])),
        &caps,
    )
    .await
    .expect("run");

    let shaped = &outcome.output["nodes"]["shape"]["items"][0]["json"];
    assert_eq!(shaped["repo"], json!("acme/api"), "supplied value");
    assert_eq!(shaped["depth"], json!(3), "declared default applied");
    assert_eq!(
        shaped["note"],
        json!(null),
        "optional input with no default resolves to null, not absent"
    );
}

#[tokio::test]
async fn resolved_inputs_are_readable_through_the_run_slice_too() {
    // `run.inputs` is the seeded location; the `inputs` scope key is lifted from
    // it. Both are part of the contract, so a jq program walking `run` works.
    let compiled = compile(&parameterized_graph()).expect("compile");
    let caps = mock_capabilities();

    let outcome = run(
        &compiled,
        RunInput::new(json!({ "from": "webhook" }))
            .with_inputs(values(&[("repo", json!("acme/api"))])),
        &caps,
    )
    .await
    .expect("run");

    assert_eq!(outcome.output["run"]["inputs"]["repo"], json!("acme/api"));
    assert_eq!(outcome.output["run"]["inputs"]["depth"], json!(3));
    // The trigger payload is a separate channel and is untouched by inputs.
    assert_eq!(
        outcome.output["run"]["trigger"],
        json!({ "from": "webhook" })
    );
}

#[tokio::test]
async fn a_missing_required_input_runs_nothing() {
    let compiled = compile(&parameterized_graph()).expect("compile");
    let caps = mock_capabilities();

    let err = run(&compiled, json!({}), &caps)
        .await
        .expect_err("a missing required input must fail the run");

    match err {
        EngineError::Input(inner) => {
            assert_eq!(inner.code(), "input_missing");
            assert_eq!(inner.input_name(), "repo");
        }
        other => panic!("expected an input error, got: {other:?}"),
    }
}

#[tokio::test]
async fn a_wrongly_typed_input_is_rejected_before_the_run() {
    let compiled = compile(&parameterized_graph()).expect("compile");
    let caps = mock_capabilities();

    let err = run(
        &compiled,
        RunInput::new(json!({})).with_inputs(values(&[
            ("repo", json!("acme/api")),
            ("depth", json!("3")),
        ])),
        &caps,
    )
    .await
    .expect_err("a string for a number input must be rejected");

    assert!(
        matches!(&err, EngineError::Input(inner) if inner.code() == "input_type_mismatch"),
        "expected a type mismatch, got: {err:?}"
    );
}

#[tokio::test]
async fn an_undeclared_input_is_rejected_rather_than_silently_dropped() {
    let compiled = compile(&parameterized_graph()).expect("compile");
    let caps = mock_capabilities();

    let err = run(
        &compiled,
        RunInput::new(json!({})).with_inputs(values(&[
            ("repo", json!("acme/api")),
            ("reop", json!("typo")),
        ])),
        &caps,
    )
    .await
    .expect_err("an undeclared key must be rejected");

    assert!(
        matches!(&err, EngineError::Input(inner) if inner.input_name() == "reop"),
        "expected the typo to be named, got: {err:?}"
    );
}

#[tokio::test]
async fn a_graph_declaring_no_inputs_still_runs_from_a_bare_payload() {
    // The historical call shape — a bare `Value` — must keep working untouched.
    let graph = WorkflowGraph {
        nodes: vec![
            trigger("t", TriggerKind::Manual),
            node(
                "shape",
                NodeKind::Transform,
                json!({ "set": { "seen": "=run.trigger.hi" } }),
            ),
        ],
        edges: vec![edge("t", "shape")],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let caps = mock_capabilities();

    let outcome = run(&compiled, json!({ "hi": 1 }), &caps)
        .await
        .expect("run");
    assert_eq!(
        outcome.output["nodes"]["shape"]["items"][0]["json"]["seen"],
        json!(1)
    );
}

#[tokio::test]
async fn a_parent_forwards_its_own_inputs_to_a_sub_workflow_child() {
    // The child declares `repo`; the parent declares `repo` too and forwards it
    // through the sub_workflow node's `inputs` config.
    let child = WorkflowGraph {
        name: "child".to_string(),
        inputs: vec![WorkflowInput::new("repo", InputType::String).required()],
        nodes: vec![
            trigger("ct", TriggerKind::Manual),
            node(
                "echo",
                NodeKind::Transform,
                json!({ "set": { "child_repo": "=inputs.repo" } }),
            ),
        ],
        edges: vec![edge("ct", "echo")],
        ..Default::default()
    };
    let caps =
        mock_capabilities_with_resolver(MockWorkflowResolver::default().with("child-1", child));

    let parent = WorkflowGraph {
        name: "parent".to_string(),
        inputs: vec![WorkflowInput::new("repo", InputType::String).required()],
        nodes: vec![
            trigger("t", TriggerKind::Manual),
            node(
                "sub",
                NodeKind::SubWorkflow,
                json!({ "workflow_id": "child-1", "inputs": { "repo": "=inputs.repo" } }),
            ),
        ],
        edges: vec![edge("t", "sub")],
        ..Default::default()
    };
    let compiled = compile(&parent).expect("compile parent");

    let outcome = run(
        &compiled,
        RunInput::new(json!({})).with_inputs(values(&[("repo", json!("acme/api"))])),
        &caps,
    )
    .await
    .expect("run parent");

    let child_state = &outcome.output["nodes"]["sub"]["items"][0]["json"];
    assert_eq!(
        child_state["nodes"]["echo"]["items"][0]["json"]["child_repo"],
        json!("acme/api"),
        "the parent's input should reach the child's `=inputs.repo`"
    );
}

#[tokio::test]
async fn a_parent_that_omits_a_required_child_input_fails() {
    let child = WorkflowGraph {
        name: "child".to_string(),
        inputs: vec![WorkflowInput::new("repo", InputType::String).required()],
        nodes: vec![trigger("ct", TriggerKind::Manual)],
        ..Default::default()
    };
    let caps =
        mock_capabilities_with_resolver(MockWorkflowResolver::default().with("child-1", child));

    let parent = WorkflowGraph {
        name: "parent".to_string(),
        nodes: vec![
            trigger("t", TriggerKind::Manual),
            node(
                "sub",
                NodeKind::SubWorkflow,
                json!({ "workflow_id": "child-1" }),
            ),
        ],
        edges: vec![edge("t", "sub")],
        ..Default::default()
    };
    let compiled = compile(&parent).expect("compile parent");

    let err = run(&compiled, json!({}), &caps)
        .await
        .expect_err("the child's requirement must be enforced across the boundary");
    assert!(
        err.to_string().contains("repo"),
        "the error should name the missing child input, got: {err}"
    );
}

#[tokio::test]
async fn a_jq_program_must_address_inputs_with_a_leading_dot() {
    // `inputs` is the one scope key that collides with a jq builtin (jq's own
    // `inputs` reads further program inputs). A simple dotted path is walked
    // directly and is fine; anything jq compiles needs the leading dot, and
    // getting it wrong yields nothing rather than erroring — which is exactly
    // why this is pinned rather than left to a doc comment.
    let graph = WorkflowGraph {
        name: "jq-inputs".to_string(),
        inputs: vec![WorkflowInput::new("repo", InputType::String).required()],
        nodes: vec![
            trigger("t", TriggerKind::Manual),
            node(
                "shape",
                NodeKind::Transform,
                json!({ "set": {
                    // The fast dotted path: no jq compilation, so bare `inputs`
                    // resolves against the scope.
                    "direct": "=inputs.repo",
                    // A real jq program, addressed correctly.
                    "dotted": "=\"repo: \" + .inputs.repo",
                    // The same program with the collision. Kept in the test on
                    // purpose: if a future change makes this resolve, the doc
                    // warning is stale and should be removed with it.
                    "bare": "=\"repo: \" + inputs.repo",
                } }),
            ),
        ],
        edges: vec![edge("t", "shape")],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("compile");
    let caps = mock_capabilities();

    let outcome = run(
        &compiled,
        RunInput::new(json!({})).with_inputs(values(&[("repo", json!("acme/api"))])),
        &caps,
    )
    .await
    .expect("run");

    let shaped = &outcome.output["nodes"]["shape"]["items"][0]["json"];
    assert_eq!(shaped["direct"], json!("acme/api"), "dotted-path fast form");
    assert_eq!(
        shaped["dotted"],
        json!("repo: acme/api"),
        "a jq program addressing `.inputs`"
    );
    assert_eq!(
        shaped["bare"],
        json!(null),
        "bare `inputs` in a jq program hits jq's builtin and yields nothing — \
         the reason authors are told to write `.inputs.<name>` there"
    );
}

#[tokio::test]
async fn a_per_item_sub_workflow_forwards_inputs_derived_from_its_own_element() {
    // The `inputs` map is resolved inside `run_child`, against the same scope
    // as `workflow_id`. For a `per_item` fan-out that scope is the *current
    // element*, so each child receives values derived from its own item rather
    // than from the batch — resolving once at the call site would give every
    // child the first element's values.
    let child = WorkflowGraph {
        name: "child".to_string(),
        inputs: vec![WorkflowInput::new("repo", InputType::String).required()],
        nodes: vec![
            trigger("ct", TriggerKind::Manual),
            node(
                "echo",
                NodeKind::Transform,
                json!({ "set": { "child_repo": "=inputs.repo" } }),
            ),
        ],
        edges: vec![edge("ct", "echo")],
        ..Default::default()
    };
    let caps =
        mock_capabilities_with_resolver(MockWorkflowResolver::default().with("child-1", child));

    let parent = WorkflowGraph {
        name: "parent".to_string(),
        nodes: vec![
            trigger("t", TriggerKind::Manual),
            // Fan the trigger payload's array out into one item per element…
            node("fan", NodeKind::SplitOut, json!({ "path": "repos" })),
            // …and run the child once per element, each with its own `repo`.
            node(
                "sub",
                NodeKind::SubWorkflow,
                json!({
                    "workflow_id": "child-1",
                    "execution": "per_item",
                    "inputs": { "repo": "=item.name" }
                }),
            ),
        ],
        edges: vec![edge("t", "fan"), edge("fan", "sub")],
        ..Default::default()
    };
    let compiled = compile(&parent).expect("compile parent");

    let outcome = run(
        &compiled,
        json!({ "repos": [{ "name": "acme/api" }, { "name": "acme/web" }] }),
        &caps,
    )
    .await
    .expect("run parent");

    let children = outcome.output["nodes"]["sub"]["items"]
        .as_array()
        .expect("one output item per element");
    assert_eq!(children.len(), 2, "both elements should have run");

    let seen: Vec<&str> = children
        .iter()
        .map(|item| {
            item["json"]["nodes"]["echo"]["items"][0]["json"]["child_repo"]
                .as_str()
                .unwrap_or("<unresolved>")
        })
        .collect();
    assert_eq!(
        seen,
        vec!["acme/api", "acme/web"],
        "each child should have received its own element's value"
    );
}
