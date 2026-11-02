//! The serialized shape of each entry is what a builder tool hands its agent, so
//! these compare against literal JSON rather than against the structs.

use super::*;
use crate::expr::NullResolution;
use serde_json::json;

fn graph(nodes: Value, edges: Value) -> WorkflowGraph {
    serde_json::from_value(json!({ "name": "t", "nodes": nodes, "edges": edges })).unwrap()
}

fn step(node_id: &str, status: StepStatus, nulls: &[(&str, &str)]) -> ExecutionStep {
    ExecutionStep {
        node_id: node_id.to_string(),
        status,
        output: json!({}),
        duration_ms: 1,
        diagnostics: nulls
            .iter()
            .map(|(l, e)| NullResolution {
                location: l.to_string(),
                expression: e.to_string(),
            })
            .collect(),
        transcript: vec![],
    }
}

fn chain(upstream_slug: &str) -> WorkflowGraph {
    graph(
        json!([
            { "id": "t", "kind": "trigger", "name": "T", "config": {} },
            { "id": "get", "kind": "tool_call", "name": "Get", "config": { "slug": upstream_slug } },
            { "id": "send", "kind": "tool_call", "name": "Send", "config": { "slug": "GMAIL_SEND" } },
        ]),
        json!([
            { "from_node": "t", "to_node": "get" },
            { "from_node": "get", "to_node": "send" },
        ]),
    )
}

#[test]
fn plain_null_arg_serializes_without_the_unverifiable_fields() {
    let g = graph(
        json!([{ "id": "send", "kind": "tool_call", "name": "Send", "config": { "slug": "X" } }]),
        json!([]),
    );
    let steps = [step(
        "send",
        StepStatus::Success,
        &[("args.to", "=item.json.to")],
    )];
    let report = AuthoringReport::build(&g, &steps, &json!({}));
    assert_eq!(
        serde_json::to_value(&report.null_resolutions).unwrap(),
        json!([{ "node_id": "send", "location": "args.to", "expression": "=item.json.to" }])
    );
    assert!(report.is_failing());
}

#[test]
fn composio_upstream_is_unverifiable_with_data_nesting_advice() {
    let g = chain("GMAIL_FETCH");
    let steps = [step(
        "send",
        StepStatus::Success,
        &[("args.to", "=nodes.get.item.json.data.x")],
    )];
    let entries = tool_call_arg_nulls(&g, &steps);
    assert_eq!(
        serde_json::to_value(&entries).unwrap(),
        json!([{
            "node_id": "send",
            "location": "args.to",
            "expression": "=nodes.get.item.json.data.x",
            "unverifiable": true,
            "upstream_tool_call": "get",
            "suggestion": "required arg `to` binds to the output of Composio tool_call node `get` — the SANDBOX only echoes tool calls and can never produce their real output fields, so this binding is UNVERIFIABLE here (not necessarily wrong). Confirm the path against get_tool_contract { slug }'s output_fields / primary_array_path (remember Composio results nest under `.item.json.data.`), or get_tool_output_sample { slug, args } for the real shape. It is a real bug only if the path doesn't match the action's actual output."
        }])
    );
    assert_eq!(unverifiable_bindings(&g, &steps).len(), 1);
}

#[test]
fn native_upstream_gets_flat_binding_advice() {
    let g = chain("oh:file_read");
    let steps = [step(
        "send",
        StepStatus::Success,
        &[("args.to", "=nodes.get.item.json.x")],
    )];
    let entries = tool_call_arg_nulls(&g, &steps);
    let suggestion = entries[0].suggestion.as_deref().unwrap();
    assert!(
        suggestion.contains("native tool_call node `get`"),
        "{suggestion}"
    );
    assert!(suggestion.contains("binds FLAT at `=nodes.get.item.json.<field>`"));
}

#[test]
fn agent_nulls_are_split_by_location() {
    let g = graph(
        json!([{ "id": "ag", "kind": "agent", "name": "A", "config": {} }]),
        json!([]),
    );
    let steps = [step(
        "ag",
        StepStatus::Success,
        &[
            ("prompt", "=item.p"),
            ("input_context", "=item.c"),
            ("other", "=x"),
        ],
    )];
    let report = AuthoringReport::build(&g, &steps, &json!({}));
    assert_eq!(report.agent_prompt_nulls.len(), 1);
    assert_eq!(report.agent_input_context_nulls.len(), 1);
    assert_eq!(
        serde_json::to_value(&report.agent_prompt_nulls[0]).unwrap(),
        json!({
            "node_id": "ag",
            "location": "prompt",
            "expression": "=item.p",
            "suggestion": "Feed upstream data via input_context:\"=item\" and make the prompt a plain instruction."
        })
    );
}

#[test]
fn hidden_tool_call_error_uses_engine_message_or_generic_text() {
    let g = chain("GMAIL_FETCH");
    let output = json!({ "nodes": { "get": { "items": [{ "json": { "error": "boom" } }] } } });
    let steps = [
        step("get", StepStatus::Error, &[]),
        step("send", StepStatus::Error, &[]),
    ];
    let report = AuthoringReport::build(&g, &steps, &output);
    assert_eq!(
        report.node_errors[0],
        NodeErrorEntry {
            node_id: "get".into(),
            error: "boom".into()
        }
    );
    assert!(
        report.node_errors[1]
            .error
            .starts_with("tool_call node 'send' failed during the sandbox run")
    );
}

#[test]
fn unexecuted_agent_or_tool_call_nodes_warn_with_the_condition_that_routed_past() {
    let g = graph(
        json!([
            { "id": "t", "kind": "trigger", "name": "T", "config": {} },
            { "id": "c", "kind": "condition", "name": "C", "config": {} },
            { "id": "n", "kind": "tool_call", "name": "N", "config": { "slug": "X" } },
            { "id": "orphan", "kind": "agent", "name": "O", "config": {} },
        ]),
        json!([
            { "from_node": "t", "to_node": "c" },
            { "from_node": "c", "to_node": "n" },
        ]),
    );
    let steps = [
        step("t", StepStatus::Success, &[]),
        step("c", StepStatus::Success, &[]),
    ];
    let report = AuthoringReport::build(&g, &steps, &json!({}));
    assert_eq!(
        serde_json::to_value(&report.routing_divergence_warnings).unwrap(),
        json!([
            {
                "node_id": "n",
                "condition_node_id": "c",
                "message": "Node 'n' did not execute in the dry run (condition 'c' routed to the other branch under mock data); verify the wiring — at runtime with real data it may route differently."
            },
            {
                "node_id": "orphan",
                "condition_node_id": null,
                "message": "Node 'orphan' did not execute in the dry run (an upstream branch routed the mock data away from it); verify the wiring — at runtime with real data it may route differently."
            }
        ])
    );
    assert!(!report.is_failing(), "routing warnings are advisory");
}

fn code_graph(source: &str, language: &str) -> WorkflowGraph {
    graph(
        json!([
            { "id": "t", "kind": "trigger", "name": "T", "config": {} },
            { "id": "shape", "kind": "code", "name": "Shape",
              "config": { "language": language, "source": source } },
        ]),
        json!([{ "from_node": "t", "to_node": "shape" }]),
    )
}

#[test]
fn every_code_node_is_reported_unexecuted_without_failing_the_run() {
    let g = code_graph("return { n: input.length };", "javascript");
    let report = AuthoringReport::build(&g, &[], &json!({}));
    let entries = serde_json::to_value(&report.unexecuted_code_nodes).unwrap();
    assert_eq!(entries.as_array().unwrap().len(), 1);
    assert_eq!(entries[0]["node_id"], "shape");
    assert!(
        entries[0]["message"]
            .as_str()
            .unwrap()
            .contains("was NOT executed")
    );
    assert!(report.code_source_errors.is_empty());
    assert!(!report.is_failing());
}

#[test]
fn a_code_source_reading_an_undefined_item_is_reported_but_does_not_fail_the_dry_run() {
    let g = code_graph("var txs = txsOf(item, 0);\nreturn txs;", "javascript");
    let report = AuthoringReport::build(&g, &[], &json!({}));
    assert_eq!(report.code_source_errors.len(), 1);
    assert_eq!(report.code_source_errors[0].node_id, "shape");
    assert!(
        report.code_source_errors[0]
            .message
            .contains("ReferenceError")
    );
    assert!(!report.is_failing());

    let py = AuthoringReport::build(&code_graph("return item", "python"), &[], &json!({}));
    assert!(py.code_source_errors[0].message.contains("NameError"));
}
