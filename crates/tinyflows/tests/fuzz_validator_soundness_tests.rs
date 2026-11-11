#![cfg(feature = "mock")]
//! Validator soundness: a refusal must name a graph that really misbehaves.
//!
//! The engine does not re-validate. `CompiledWorkflow` is a plain struct with
//! one public field, so a refused graph can be forced past `compile` and run.
//! Each case here first proves the refusal is real (`compile` fails with the
//! expected error), then runs the forced graph and asserts the specific wrong
//! behaviour the refusal exists to prevent. A refusal whose forced run behaves
//! correctly would be superstition; these tests would then fail and say so.
//!
//! Only the top-level graph can be forced: `sub_workflow` and `spawn` compile
//! their children at run time.

mod support;

use std::panic::AssertUnwindSafe;
use std::time::Duration;

use proptest::prelude::*;
use serde_json::{Value, json};

use support::graphgen::{Shape, arb_shape, graph_of};
use tinyflows::caps::mock::mock_capabilities;
use tinyflows::compiler::{CompiledWorkflow, compile};
use tinyflows::engine::run;
use tinyflows::error::ValidationError;
use tinyflows::model::{Edge, Node, NodeKind, WorkflowGraph};
use tinyflows::validate::validate_all;

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
    port_edge(from, "main", to)
}

fn port_edge(from: &str, port: &str, to: &str) -> Edge {
    Edge {
        from_node: from.to_string(),
        from_port: port.to_string(),
        to_node: to.to_string(),
        to_port: "main".to_string(),
    }
}

fn graph(name: &str, nodes: Vec<Node>, edges: Vec<Edge>) -> WorkflowGraph {
    WorkflowGraph {
        name: name.to_string(),
        nodes,
        edges,
        ..Default::default()
    }
}

/// What a forced run did.
#[derive(Debug)]
enum Forced {
    Ok(Value),
    Err(String),
    Hung,
    Panicked,
}

/// Runs `graph` with no validation at all.
fn force(graph: &WorkflowGraph, input: Value) -> Forced {
    let compiled = CompiledWorkflow {
        graph: graph.clone(),
    };
    let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build runtime")
            .block_on(async {
                tokio::time::timeout(GUARD, run(&compiled, input, &mock_capabilities())).await
            })
    }));
    match outcome {
        Err(_) => Forced::Panicked,
        Ok(Err(_)) => Forced::Hung,
        Ok(Ok(Err(error))) => Forced::Err(error.to_string()),
        Ok(Ok(Ok(outcome))) => Forced::Ok(outcome.output),
    }
}

/// Asserts `graph` is refused, and refused for the reason under test.
fn assert_refused(graph: &WorkflowGraph, reason: impl Fn(&ValidationError) -> bool) {
    assert!(compile(graph).is_err(), "{} was not refused", graph.name);
    let errors = validate_all(graph);
    assert!(
        errors.iter().any(reason),
        "{} was refused for the wrong reason: {errors:?}",
        graph.name
    );
}

fn refused_at(node_id: &'static str) -> impl Fn(&ValidationError) -> bool {
    move |error| matches!(error, ValidationError::InvalidNodeConfig { node, .. } if node == node_id)
}

/// An off-cycle arm into a merge on the cycle: from the second pass the barrier
/// can never complete, so the loop stops dead and never leaves through `done`.
#[test]
fn a_forced_off_cycle_merge_stops_the_loop_dead() {
    let g = graph(
        "off_cycle_arm",
        vec![
            node("t", NodeKind::Trigger, json!({ "recursion_limit": 100 })),
            node("outside", NodeKind::OutputParser, Value::Null),
            node(
                "l",
                NodeKind::Loop,
                json!({ "max_iterations": 3, "on_exceeded": "continue" }),
            ),
            node("work", NodeKind::OutputParser, Value::Null),
            node("join", NodeKind::Merge, Value::Null),
            node("out", NodeKind::OutputParser, Value::Null),
        ],
        vec![
            edge("t", "l"),
            edge("t", "outside"),
            port_edge("l", "body", "work"),
            edge("work", "join"),
            edge("outside", "join"),
            edge("join", "l"),
            port_edge("l", "done", "out"),
        ],
    );
    assert_refused(
        &g,
        |e| matches!(e, ValidationError::IllegalCycle(id) if id == "join"),
    );
    if let Forced::Ok(output) = force(&g, json!({})) {
        assert!(
            output["nodes"]["out"].is_null(),
            "the loop completed despite the off-cycle barrier: {output}"
        );
        assert!(
            output["nodes"]["l"]["iteration"].as_u64().unwrap_or(0) < 3,
            "the loop ran every pass despite the off-cycle barrier: {output}"
        );
    }
}

/// Condition branches labelled on `to_port` share `from_port: main`, which the
/// engine lowers as a fan-out: both branches run, whatever the condition says.
#[test]
fn a_forced_condition_on_to_port_runs_both_branches() {
    let labelled = |to: &str, label: &str| Edge {
        from_node: "if".to_string(),
        from_port: "main".to_string(),
        to_node: to.to_string(),
        to_port: label.to_string(),
    };
    let g = graph(
        "condition_on_to_port",
        vec![
            node("t", NodeKind::Trigger, Value::Null),
            node("if", NodeKind::Condition, json!({ "field": "flag" })),
            node("yes", NodeKind::OutputParser, Value::Null),
            node("no", NodeKind::OutputParser, Value::Null),
        ],
        vec![
            edge("t", "if"),
            labelled("yes", "true"),
            labelled("no", "false"),
        ],
    );
    assert_refused(
        &g,
        |e| matches!(e, ValidationError::InvalidConditionRouting { node, .. } if node == "if"),
    );
    match force(&g, json!({ "flag": true })) {
        Forced::Ok(output) => {
            assert!(
                !output["nodes"]["yes"].is_null(),
                "yes did not run: {output}"
            );
            assert!(
                !output["nodes"]["no"].is_null(),
                "the untaken branch did not run, so this refusal may be superstitious: {output}"
            );
        }
        other => panic!("expected a (wrongly) successful fan-out, got {other:?}"),
    }
}

/// A gather nobody scatters into. The validator says it would wait out its poll
/// budget; by reading `gather.rs`, `expected_lanes` falls back to the arrival
/// count (0), so it releases at once with nothing. Either way the work upstream
/// of it is lost.
#[test]
fn a_forced_gather_without_a_scatter_loses_its_input() {
    let g = graph(
        "orphan_gather",
        vec![
            node("t", NodeKind::Trigger, json!({ "recursion_limit": 100 })),
            node(
                "work",
                NodeKind::Transform,
                json!({ "set": { "done": true } }),
            ),
            node(
                "gather",
                NodeKind::Gather,
                json!({ "from": ["work"], "poll_interval_ms": 1, "max_polls": 5 }),
            ),
        ],
        vec![edge("t", "work"), edge("work", "gather")],
    );
    assert_refused(&g, refused_at("gather"));
    if let Forced::Ok(output) = force(&g, json!({})) {
        assert_eq!(
            output["nodes"]["work"]["items"].as_array().map(Vec::len),
            Some(1),
            "work should have run: {output}"
        );
        assert_eq!(
            output["nodes"]["gather"]["items"]
                .as_array()
                .map(Vec::len)
                .unwrap_or(0),
            0,
            "a gather with no scatter collected its input, so this refusal may be superstitious"
        );
    }
}

/// A scatter with no gather: the lanes run, but a lane never writes the node's
/// top-level slot, so their output is invisible to the rest of the run.
#[test]
fn a_forced_scatter_without_a_gather_strands_its_lanes() {
    let g = graph(
        "ungathered_scatter",
        vec![
            node("t", NodeKind::Trigger, json!({ "recursion_limit": 100 })),
            node("scatter", NodeKind::Scatter, json!({ "path": "rows" })),
            node(
                "work",
                NodeKind::Transform,
                json!({ "set": { "done": true } }),
            ),
        ],
        vec![edge("t", "scatter"), edge("scatter", "work")],
    );
    assert_refused(&g, refused_at("scatter"));
    match force(&g, json!({ "rows": [1, 2, 3] })) {
        Forced::Ok(output) => {
            assert_eq!(
                output["nodes"]["work"]["lanes"]
                    .as_object()
                    .map(serde_json::Map::len),
                Some(3),
                "every lane should have run: {output}"
            );
            assert!(
                output["nodes"]["work"].get("items").is_none(),
                "the lane output became visible, so this refusal may be superstitious: {output}"
            );
        }
        other => panic!("expected lanes that run and are stranded, got {other:?}"),
    }
}

/// A scatter inside a lane is refused statically and backstopped at run time:
/// the inner scatter fails rather than mis-collecting.
#[test]
fn a_forced_nested_scatter_fails_inside_its_lane() {
    let g = graph(
        "nested_scatter",
        vec![
            node("t", NodeKind::Trigger, json!({ "recursion_limit": 100 })),
            node("outer", NodeKind::Scatter, json!({ "path": "rows" })),
            node("inner", NodeKind::Scatter, json!({ "path": "parts" })),
            node(
                "work",
                NodeKind::Transform,
                json!({ "set": { "done": true } }),
            ),
            // `timeout_partial` so the run settles however many lanes arrive,
            // leaving the failed inner scatter's lane slot readable in state.
            node(
                "gather",
                NodeKind::Gather,
                json!({
                    "from": ["work"],
                    "release": "timeout_partial",
                    "poll_interval_ms": 1,
                    "max_polls": 5,
                }),
            ),
        ],
        vec![
            edge("t", "outer"),
            edge("outer", "inner"),
            edge("inner", "work"),
            edge("work", "gather"),
        ],
    );
    assert_refused(&g, refused_at("inner"));
    let rows = json!({ "rows": [{ "parts": [1, 2] }, { "parts": [3] }] });
    let reported = match force(&g, rows) {
        Forced::Ok(output) => output.to_string(),
        Forced::Err(error) => error,
        other => panic!("expected the nested-scatter backstop, got {other:?}"),
    };
    assert!(
        reported.contains("nested scatter is not supported"),
        "the nested scatter ran: {reported}"
    );
}

/// A loop head inside a lane: re-entry is counted in the node's top-level slot,
/// which a lane never writes, so the counter never advances and the loop never
/// finishes its two passes per lane.
#[test]
fn a_forced_loop_in_a_lane_never_counts_its_passes() {
    let g = graph(
        "loop_in_lane",
        vec![
            node(
                "t",
                NodeKind::Trigger,
                json!({ "recursion_limit": 80, "max_node_visits": 40 }),
            ),
            node("scatter", NodeKind::Scatter, json!({ "path": "rows" })),
            node(
                "l",
                NodeKind::Loop,
                json!({ "max_iterations": 2, "on_exceeded": "continue" }),
            ),
            node("w", NodeKind::OutputParser, Value::Null),
            node(
                "gather",
                NodeKind::Gather,
                json!({ "from": ["l"], "poll_interval_ms": 1, "max_polls": 5 }),
            ),
        ],
        vec![
            edge("t", "scatter"),
            edge("scatter", "l"),
            port_edge("l", "body", "w"),
            edge("w", "l"),
            port_edge("l", "done", "gather"),
        ],
    );
    assert_refused(&g, refused_at("l"));
    if let Forced::Ok(output) = force(&g, json!({ "rows": [1, 2] })) {
        let gathered = output["nodes"]["gather"]["items"].as_array().map(Vec::len);
        assert_ne!(
            gathered,
            Some(2),
            "both lanes looped and exited correctly, so this refusal may be superstitious: {output}"
        );
    }
}

/// An edge to a node that does not exist: the runtime graph refuses to build.
#[test]
fn a_forced_edge_to_an_unknown_node_fails_the_run() {
    let g = graph(
        "ghost_edge",
        vec![
            node("t", NodeKind::Trigger, Value::Null),
            node("a", NodeKind::OutputParser, Value::Null),
        ],
        vec![edge("t", "a"), edge("a", "ghost")],
    );
    assert_refused(&g, |e| matches!(e, ValidationError::UnknownNode(_)));
    assert!(
        !matches!(force(&g, json!({})), Forced::Ok(_)),
        "a graph with a dangling edge ran to completion"
    );
}

/// No trigger: there is no entry point to run from.
#[test]
fn a_forced_graph_without_a_trigger_fails_the_run() {
    let g = graph(
        "no_trigger",
        vec![node("a", NodeKind::OutputParser, Value::Null)],
        vec![],
    );
    assert_refused(&g, |e| matches!(e, ValidationError::MissingTrigger));
    assert!(
        matches!(force(&g, json!({})), Forced::Err(_)),
        "a graph with no trigger did not fail cleanly"
    );
}

/// Rewrites the root condition's branch labels from `from_port` onto
/// `to_port`. Returns the id of the `true` arm's entry node, which a correct run
/// of a graphgen branch never reaches (its field is always absent).
fn label_on_to_port(graph: &mut WorkflowGraph, condition: &str) -> String {
    let mut yes = String::new();
    for edge in graph.edges.iter_mut().filter(|e| e.from_node == condition) {
        if edge.from_port == "true" {
            yes = edge.to_node.clone();
        }
        edge.to_port = std::mem::replace(&mut edge.from_port, "main".to_string());
    }
    yes
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, ..ProptestConfig::default() })]

    /// For every runnable generated branch, mislabelling its condition is
    /// refused, and the forced run takes the arm a correct run never takes.
    #[test]
    fn a_mislabelled_generated_condition_runs_its_untaken_arm(
        yes in arb_shape(1),
        no in arb_shape(1),
    ) {
        let valid = graph_of(&Shape::Branch(Box::new(yes), Box::new(no)));
        if compile(&valid).is_err() {
            return Ok(());
        }
        // The root branch is the trigger's only successor.
        let condition = valid
            .edges
            .iter()
            .find(|e| e.from_node == "trigger")
            .map(|e| e.to_node.clone())
            .expect("trigger edge");
        let mut forced = valid.clone();
        let yes_entry = label_on_to_port(&mut forced, &condition);
        prop_assert!(!yes_entry.is_empty(), "the root branch has no true arm");

        let errors = validate_all(&forced);
        prop_assert!(compile(&forced).is_err(), "the mislabelled graph was accepted");
        prop_assert!(
            errors.iter().all(|e| matches!(e, ValidationError::InvalidConditionRouting { .. })),
            "the mutation introduced another refusal: {:?}", errors
        );

        if let Forced::Ok(output) = force(&valid, json!({})) {
            prop_assert!(output["nodes"][&yes_entry].is_null(), "baseline took the true arm");
        }
        let result = force(&forced, json!({}));
        prop_assert!(
            !matches!(result, Forced::Hung | Forced::Panicked),
            "the forced run did not settle: {:?}", result
        );
        if let Forced::Ok(output) = result {
            prop_assert!(
                !output["nodes"][&yes_entry].is_null(),
                "the forced run did not take the untaken arm {}: {}", yes_entry, output
            );
        }
    }
}
