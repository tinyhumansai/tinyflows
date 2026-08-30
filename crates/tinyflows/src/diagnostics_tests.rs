//! Tests for reading a simulation's steps.
//!
//! The steps are synthesised rather than produced by a real run: what is under
//! test is the *reading*, and building a graph that makes the engine emit a
//! particular diagnostic would be testing the engine instead. The one case that
//! needs a real run — that a green outcome can still hide a null — is covered
//! end-to-end in `ops_tests`.

use super::*;
use serde_json::json;

fn graph(nodes: serde_json::Value, edges: serde_json::Value) -> WorkflowGraph {
    serde_json::from_value(json!({ "name": "test", "nodes": nodes, "edges": edges }))
        .expect("graph parses")
}

fn step(node_id: &str, nulls: &[(&str, &str)]) -> ExecutionStep {
    ExecutionStep {
        node_id: node_id.to_string(),
        status: StepStatus::Success,
        output: json!({}),
        duration_ms: 1,
        diagnostics: nulls
            .iter()
            .map(|(location, expression)| crate::expr::NullResolution {
                location: location.to_string(),
                expression: expression.to_string(),
            })
            .collect(),
        transcript: vec![],
    }
}

fn failed(node_id: &str, output: serde_json::Value) -> ExecutionStep {
    ExecutionStep {
        node_id: node_id.to_string(),
        status: StepStatus::Error,
        output,
        duration_ms: 1,
        // The point of this case: an error hidden by an `on_error` policy
        // carries no diagnostics at all.
        diagnostics: Vec::new(),
        transcript: vec![],
    }
}

// ---- null bindings ----

#[test]
fn a_binding_that_resolved_to_null_is_reported_with_where_it_was() {
    let graph = graph(
        json!([
            { "id": "shape", "kind": "transform", "name": "Shape", "config": {} },
            { "id": "notify", "kind": "tool_call", "name": "Notify", "config": {} },
        ]),
        json!([]),
    );

    let diagnosis = diagnose(
        &graph,
        &[step("notify", &[("args.text", "=nodes.shape.item.title")])],
    );

    assert_eq!(diagnosis.null_bindings.len(), 1);
    assert_eq!(diagnosis.null_bindings[0].node_id, "notify");
    assert_eq!(diagnosis.null_bindings[0].location, "args.text");
    assert_eq!(
        diagnosis.null_bindings[0].reads_from.as_deref(),
        Some("shape")
    );
    assert!(!diagnosis.is_clean(), "a null binding is not a clean run");
}

#[test]
fn a_null_reading_from_an_agent_is_marked_unverifiable_and_does_not_fail_the_run() {
    let graph = graph(
        json!([
            { "id": "fetch", "kind": "agent", "name": "Fetch", "config": { "prompt": "go" } },
            { "id": "notify", "kind": "tool_call", "name": "Notify", "config": {} },
        ]),
        json!([]),
    );

    let diagnosis = diagnose(
        &graph,
        &[step(
            "notify",
            &[("args.text", "=nodes.fetch.item.json.title")],
        )],
    );

    // The sandbox stands in for a harness with a canned reply, so a null here
    // says the mock had no such field — not that a real session would not
    // produce one. Failing on it makes an agent re-wire a correct graph.
    assert!(diagnosis.null_bindings[0].unverifiable);
    assert!(
        diagnosis.null_bindings[0]
            .suggestion
            .contains("cannot confirm")
    );
    assert!(diagnosis.is_clean(), "an unverifiable null must not block");
}

#[test]
fn a_null_that_reads_from_nothing_is_still_reported_as_checkable() {
    let graph = graph(
        json!([{ "id": "notify", "kind": "tool_call", "name": "Notify", "config": {} }]),
        json!([]),
    );

    let diagnosis = diagnose(
        &graph,
        &[step("notify", &[("args.text", "=run.trigger.x")])],
    );

    assert!(!diagnosis.null_bindings[0].unverifiable);
    assert_eq!(diagnosis.null_bindings[0].reads_from, None);
    assert!(!diagnosis.is_clean());
}

// ---- empty prompts ----

#[test]
fn an_agent_whose_instruction_resolved_to_null_is_its_own_class() {
    let graph = graph(
        json!([{ "id": "work", "kind": "agent", "name": "Work", "config": {} }]),
        json!([]),
    );

    let diagnosis = diagnose(
        &graph,
        &[step("work", &[("prompt", "=nodes.missing.item.x")])],
    );

    // Not a generic null: this node still runs, and dispatches a whole harness
    // session with nothing to do.
    assert_eq!(diagnosis.empty_prompts, vec!["work".to_string()]);
    assert!(diagnosis.null_bindings.is_empty());
    assert!(!diagnosis.is_clean());
}

#[test]
fn the_instruction_alias_counts_as_a_prompt_too() {
    let graph = graph(
        json!([{ "id": "work", "kind": "agent", "name": "Work", "config": {} }]),
        json!([]),
    );

    let diagnosis = diagnose(&graph, &[step("work", &[("instruction", "=.item.text")])]);

    assert_eq!(diagnosis.empty_prompts, vec!["work".to_string()]);
}

#[test]
fn a_null_prompt_on_a_node_that_is_not_an_agent_is_an_ordinary_null() {
    let graph = graph(
        json!([{ "id": "shape", "kind": "transform", "name": "Shape", "config": {} }]),
        json!([]),
    );

    let diagnosis = diagnose(&graph, &[step("shape", &[("prompt", "=.item.x")])]);

    assert!(diagnosis.empty_prompts.is_empty());
    assert_eq!(diagnosis.null_bindings.len(), 1);
}

// ---- hidden errors ----

#[test]
fn a_failure_the_error_policy_swallowed_is_surfaced_with_its_message() {
    let graph = graph(
        json!([{ "id": "notify", "kind": "tool_call", "name": "Notify", "config": {} }]),
        json!([]),
    );

    let diagnosis = diagnose(
        &graph,
        &[failed("notify", json!({ "error": "slug not allowlisted" }))],
    );

    // The step carries no diagnostics, so a check that only read those would
    // report this run clean — with a node that failed in it.
    assert_eq!(diagnosis.hidden_errors.len(), 1);
    assert_eq!(diagnosis.hidden_errors[0].node_id, "notify");
    assert_eq!(
        diagnosis.hidden_errors[0].message.as_deref(),
        Some("slug not allowlisted")
    );
    assert!(!diagnosis.is_clean());
}

#[test]
fn a_failure_with_no_readable_message_is_still_reported() {
    let graph = graph(
        json!([{ "id": "notify", "kind": "tool_call", "name": "Notify", "config": {} }]),
        json!([]),
    );

    let diagnosis = diagnose(&graph, &[failed("notify", json!(null))]);

    assert_eq!(diagnosis.hidden_errors.len(), 1);
    assert_eq!(diagnosis.hidden_errors[0].message, None);
    assert!(
        !diagnosis.is_clean(),
        "a failure without a message is still a failure"
    );
}

// ---- nodes that never ran ----

#[test]
fn a_node_a_condition_routed_past_names_the_condition_that_decided() {
    let graph = graph(
        json!([
            { "id": "t", "kind": "trigger", "name": "Start",
              "config": { "trigger_kind": "manual" } },
            { "id": "check", "kind": "condition", "name": "Check",
              "config": { "expression": "=.item.ok" } },
            { "id": "yes", "kind": "agent", "name": "Yes", "config": { "prompt": "go" } },
            { "id": "no", "kind": "agent", "name": "No", "config": { "prompt": "stop" } },
        ]),
        json!([
            { "from_node": "t", "to_node": "check" },
            { "from_node": "check", "from_port": "true", "to_node": "yes" },
            { "from_node": "check", "from_port": "false", "to_node": "no" },
        ]),
    );

    let diagnosis = diagnose(
        &graph,
        &[step("t", &[]), step("check", &[]), step("yes", &[])],
    );

    // "`no` never ran" sends an author to look at `no`. Naming the condition
    // sends them to the node that actually decided.
    assert_eq!(diagnosis.never_ran.len(), 1);
    assert_eq!(diagnosis.never_ran[0].node_id, "no");
    assert_eq!(diagnosis.never_ran[0].routed_by.as_deref(), Some("check"));
    // A condition sending one sample down one branch is what a condition is
    // for, so this warns without failing.
    assert!(diagnosis.is_clean());
}

#[test]
fn only_nodes_that_do_outside_work_are_reported_as_skipped() {
    let graph = graph(
        json!([
            { "id": "check", "kind": "condition", "name": "Check",
              "config": { "expression": "=.item.ok" } },
            { "id": "shape", "kind": "transform", "name": "Shape", "config": {} },
        ]),
        json!([{ "from_node": "check", "to_node": "shape" }]),
    );

    let diagnosis = diagnose(&graph, &[step("check", &[])]);

    // A transform that was routed past is not a surprise worth a warning.
    assert!(diagnosis.never_ran.is_empty(), "{:?}", diagnosis.never_ran);
}

#[test]
fn a_skipped_node_with_no_condition_above_it_reports_no_culprit_rather_than_guessing() {
    let graph = graph(
        json!([
            { "id": "t", "kind": "trigger", "name": "Start",
              "config": { "trigger_kind": "manual" } },
            { "id": "work", "kind": "agent", "name": "Work", "config": { "prompt": "go" } },
        ]),
        json!([{ "from_node": "t", "to_node": "work" }]),
    );

    let diagnosis = diagnose(&graph, &[step("t", &[])]);

    assert_eq!(diagnosis.never_ran.len(), 1);
    assert_eq!(diagnosis.never_ran[0].routed_by, None);
}

#[test]
fn the_search_for_a_routing_condition_terminates_on_a_cycle() {
    // Not a graph the engine would run, but the walk must not hang on one:
    // this function is reached from an authoring tool, on whatever was written.
    let graph = graph(
        json!([
            { "id": "a", "kind": "transform", "name": "A", "config": {} },
            { "id": "b", "kind": "transform", "name": "B", "config": {} },
            { "id": "work", "kind": "agent", "name": "Work", "config": { "prompt": "go" } },
        ]),
        json!([
            { "from_node": "a", "to_node": "b" },
            { "from_node": "b", "to_node": "a" },
            { "from_node": "b", "to_node": "work" },
        ]),
    );

    let diagnosis = diagnose(&graph, &[]);

    assert_eq!(diagnosis.never_ran[0].routed_by, None);
}

// ---- the whole picture ----

#[test]
fn a_run_with_nothing_to_report_is_clean() {
    let graph = graph(
        json!([{ "id": "work", "kind": "agent", "name": "Work", "config": { "prompt": "go" } }]),
        json!([]),
    );

    let diagnosis = diagnose(&graph, &[step("work", &[])]);

    assert!(diagnosis.is_clean());
    assert_eq!(diagnosis, Diagnosis::default());
}
