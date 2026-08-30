#![cfg(feature = "mock")]
//! End-to-end integration tests for conditional routing.
//!
//! These build real [`WorkflowGraph`]s, compile them, and drive them against the
//! deterministic [mock capabilities](tinyflows::caps::mock), asserting that
//! `condition` and `switch` nodes send data down *only* the branch(es) their
//! routing decision selects — and that non-selected branch nodes never execute
//! (their run-state slot stays `null`).
//!
//! Data-flow reminder (see `src/engine.rs`): the trigger's items slot is seeded
//! with a single item carrying the run input, so a branching node placed directly
//! after the trigger reads its routing field off that input item. A `condition`
//! and `switch` pass their input through unchanged on the chosen port.
//!
//! Gated behind the `mock` cargo feature, so plain `cargo test` skips it while
//! `cargo test --all-features` runs it.

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

/// A two-way `condition` on the trigger's input field `go`:
/// `trigger -> condition(field=go) -> {true -> yes | false -> no}`. Both leaves
/// are `tool_call`s that echo a distinct slug, so we can tell which branch ran.
fn condition_graph() -> WorkflowGraph {
    WorkflowGraph {
        name: "condition_branch".to_string(),
        nodes: vec![
            trigger("start"),
            node("branch", NodeKind::Condition, json!({ "field": "go" })),
            node(
                "yes",
                NodeKind::ToolCall,
                json!({ "slug": "slack.notify", "args": { "path": "true" } }),
            ),
            node(
                "no",
                NodeKind::ToolCall,
                json!({ "slug": "slack.skip", "args": { "path": "false" } }),
            ),
        ],
        edges: vec![
            edge("start", "main", "branch"),
            edge("branch", "true", "yes"),
            edge("branch", "false", "no"),
        ],
        ..Default::default()
    }
}

#[tokio::test]
async fn condition_routes_truthy_input_to_true_branch() {
    let compiled = compile(&condition_graph()).expect("compile");
    let outcome = run(&compiled, json!({ "go": true }), &mock_capabilities())
        .await
        .expect("run");
    let out = &outcome.output;

    // Only the `true` branch ran and echoed its slug.
    assert_eq!(
        out["nodes"]["yes"]["items"][0]["json"]["json"]["tool"],
        "slack.notify"
    );
    // The `false` branch never executed, so it has no slot in the run state.
    assert!(
        out["nodes"]["no"].is_null(),
        "the false branch must not run for truthy input"
    );
}

#[tokio::test]
async fn condition_routes_falsey_input_to_false_branch() {
    let compiled = compile(&condition_graph()).expect("compile");
    let outcome = run(&compiled, json!({ "go": false }), &mock_capabilities())
        .await
        .expect("run");
    let out = &outcome.output;

    // Only the `false` branch ran.
    assert_eq!(
        out["nodes"]["no"]["items"][0]["json"]["json"]["tool"],
        "slack.skip"
    );
    // The `true` branch never executed.
    assert!(
        out["nodes"]["yes"].is_null(),
        "the true branch must not run for falsey input"
    );
}

/// A `switch` keyed on the input field `kind` with three explicit case ports
/// (`a`, `b`, `c`) plus a `default` port. Each leaf echoes a distinct slug.
fn switch_graph() -> WorkflowGraph {
    WorkflowGraph {
        name: "switch_branch".to_string(),
        nodes: vec![
            trigger("start"),
            node("route", NodeKind::Switch, json!({ "field": "kind" })),
            node("leaf_a", NodeKind::ToolCall, json!({ "slug": "case.a" })),
            node("leaf_b", NodeKind::ToolCall, json!({ "slug": "case.b" })),
            node("leaf_c", NodeKind::ToolCall, json!({ "slug": "case.c" })),
            node(
                "leaf_default",
                NodeKind::ToolCall,
                json!({ "slug": "case.default" }),
            ),
        ],
        edges: vec![
            edge("start", "main", "route"),
            edge("route", "a", "leaf_a"),
            edge("route", "b", "leaf_b"),
            edge("route", "c", "leaf_c"),
            edge("route", "default", "leaf_default"),
        ],
        ..Default::default()
    }
}

#[tokio::test]
async fn switch_routes_each_of_three_cases() {
    // Run the same graph once per case value and assert exactly that leaf ran.
    for (kind, ran, slug) in [
        ("a", "leaf_a", "case.a"),
        ("b", "leaf_b", "case.b"),
        ("c", "leaf_c", "case.c"),
    ] {
        let compiled = compile(&switch_graph()).expect("compile");
        let outcome = run(&compiled, json!({ "kind": kind }), &mock_capabilities())
            .await
            .expect("run");
        let out = &outcome.output;

        assert_eq!(
            out["nodes"][ran]["items"][0]["json"]["json"]["tool"], slug,
            "case {kind} should route to {ran}"
        );
        // Every other leaf, including default, must be untouched.
        for other in ["leaf_a", "leaf_b", "leaf_c", "leaf_default"] {
            if other != ran {
                assert!(
                    out["nodes"][other].is_null(),
                    "case {kind}: {other} must not run"
                );
            }
        }
    }
}

#[tokio::test]
async fn switch_routes_no_match_to_default() {
    // No `kind` field at all: the switch's key resolves to null, which routes to
    // the `default` port.
    let compiled = compile(&switch_graph()).expect("compile");
    let outcome = run(
        &compiled,
        json!({ "other": "unknown" }),
        &mock_capabilities(),
    )
    .await
    .expect("run");
    let out = &outcome.output;

    assert_eq!(
        out["nodes"]["leaf_default"]["items"][0]["json"]["json"]["tool"],
        "case.default"
    );
    for other in ["leaf_a", "leaf_b", "leaf_c"] {
        assert!(
            out["nodes"][other].is_null(),
            "no-match run: {other} must not run"
        );
    }
}

/// A two-level nested branch:
/// `trigger -> outer(condition=outer) ->`
///   `true  -> inner(condition=inner) -> {true -> tt | false -> tf}`
///   `false -> ff`.
/// The inner condition reads its field off the item the outer condition passed
/// through, so nesting composes.
fn nested_graph() -> WorkflowGraph {
    WorkflowGraph {
        name: "nested_branch".to_string(),
        nodes: vec![
            trigger("start"),
            node("outer", NodeKind::Condition, json!({ "field": "outer" })),
            node("inner", NodeKind::Condition, json!({ "field": "inner" })),
            node("tt", NodeKind::ToolCall, json!({ "slug": "leaf.tt" })),
            node("tf", NodeKind::ToolCall, json!({ "slug": "leaf.tf" })),
            node("ff", NodeKind::ToolCall, json!({ "slug": "leaf.ff" })),
        ],
        edges: vec![
            edge("start", "main", "outer"),
            edge("outer", "true", "inner"),
            edge("outer", "false", "ff"),
            edge("inner", "true", "tt"),
            edge("inner", "false", "tf"),
        ],
        ..Default::default()
    }
}

#[tokio::test]
async fn nested_branch_reaches_inner_true_leaf() {
    let compiled = compile(&nested_graph()).expect("compile");
    let outcome = run(
        &compiled,
        json!({ "outer": true, "inner": true }),
        &mock_capabilities(),
    )
    .await
    .expect("run");
    let out = &outcome.output;

    assert_eq!(
        out["nodes"]["tt"]["items"][0]["json"]["json"]["tool"],
        "leaf.tt"
    );
    assert!(out["nodes"]["tf"].is_null(), "tf must not run");
    assert!(out["nodes"]["ff"].is_null(), "ff must not run");
}

#[tokio::test]
async fn nested_branch_reaches_inner_false_leaf() {
    let compiled = compile(&nested_graph()).expect("compile");
    let outcome = run(
        &compiled,
        json!({ "outer": true, "inner": false }),
        &mock_capabilities(),
    )
    .await
    .expect("run");
    let out = &outcome.output;

    assert_eq!(
        out["nodes"]["tf"]["items"][0]["json"]["json"]["tool"],
        "leaf.tf"
    );
    assert!(out["nodes"]["tt"].is_null(), "tt must not run");
    assert!(out["nodes"]["ff"].is_null(), "ff must not run");
}

#[tokio::test]
async fn nested_branch_short_circuits_at_outer_false() {
    let compiled = compile(&nested_graph()).expect("compile");
    let outcome = run(&compiled, json!({ "outer": false }), &mock_capabilities())
        .await
        .expect("run");
    let out = &outcome.output;

    assert_eq!(
        out["nodes"]["ff"]["items"][0]["json"]["json"]["tool"],
        "leaf.ff"
    );
    // The whole inner sub-branch never executed.
    assert!(out["nodes"]["inner"].is_null(), "inner must not run");
    assert!(out["nodes"]["tt"].is_null(), "tt must not run");
    assert!(out["nodes"]["tf"].is_null(), "tf must not run");
}
