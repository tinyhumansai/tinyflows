//! Tests for the run trace.
//!
//! Driven through real runs rather than synthesised frames: what is under test
//! is that the trace reflects what the engine actually did, and hand-built
//! frames would only test the assembly code.

use super::*;
use crate::caps::mock::mock_capabilities;
use crate::compiler::compile;
use crate::engine::{CancellationToken, run_intercepted};
use crate::model::{Edge, TriggerKind};
use crate::observability::RunObserver;
use crate::testkit::Respond;
use std::sync::Arc;

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

fn edge(from_node: &str, to_node: &str) -> Edge {
    Edge {
        from_node: from_node.to_string(),
        from_port: "main".to_string(),
        to_node: to_node.to_string(),
        to_port: "main".to_string(),
    }
}

/// A graph whose tool node binds to a field nothing upstream produces — the
/// shape of a workflow that runs green and does nothing.
fn graph_with_a_null_binding() -> WorkflowGraph {
    WorkflowGraph {
        name: "traced".to_string(),
        nodes: vec![
            node(
                "t",
                NodeKind::Trigger,
                json!({ "kind": TriggerKind::Manual }),
            ),
            node(
                "call",
                NodeKind::ToolCall,
                json!({ "slug": "svc.do", "args": { "to": "=nodes.t.item.missing" } }),
            ),
        ],
        edges: vec![edge("t", "call")],
        ..Default::default()
    }
}

async fn trace_of(graph: &WorkflowGraph, mocks: Option<Arc<MockCaps>>) -> RunTrace {
    let compiled = compile(graph).expect("compile");
    let mut tracer = RunTracer::new(Some(graph.clone()));
    if let Some(mocks) = mocks.clone() {
        tracer = tracer.with_mocks(mocks);
    }
    let tracer = Arc::new(tracer);
    let caps = match mocks.as_ref() {
        Some(mocks) => mocks.capabilities(),
        None => mock_capabilities(),
    };
    run_intercepted(
        &compiled,
        json!({ "seeded": true }),
        &caps,
        &(tracer.clone() as Arc<dyn RunObserver>),
        CancellationToken::new(),
        tracer.clone(),
    )
    .await
    .expect("run");
    tracer.trace()
}

#[tokio::test]
async fn a_trace_records_every_activation_with_its_input_and_output() {
    let trace = trace_of(&graph_with_a_null_binding(), None).await;

    let call = trace.steps_for("call");
    assert_eq!(call.len(), 1, "one activation of the tool node");
    let step = call[0];
    assert_eq!(step.node_id, "call");
    assert_eq!(step.status, TraceStatus::Success);
    assert!(
        !step.input.is_empty(),
        "the node's resolved input should be recorded, not just its output"
    );
    assert!(!step.output.is_empty());
}

#[tokio::test]
async fn a_null_binding_is_recorded_with_the_node_it_reads_from() {
    // The point of the whole module: the run is green, and the trace still says
    // which binding was empty and where it was reading from.
    let trace = trace_of(&graph_with_a_null_binding(), None).await;

    let nulls = trace.null_bindings();
    assert_eq!(nulls.len(), 1, "expected exactly one null binding");
    let (node_id, binding) = nulls[0];
    assert_eq!(node_id, "call");
    assert_eq!(binding.location, "args.to");
    assert_eq!(binding.expression, "=nodes.t.item.missing");
    assert!(binding.is_null);
    assert_eq!(
        binding.reads_from.as_deref(),
        Some("t"),
        "a null binding must point at the upstream node it read from"
    );
}

#[tokio::test]
async fn a_binding_that_resolves_records_its_value() {
    let graph = WorkflowGraph {
        name: "resolves".to_string(),
        nodes: vec![
            node(
                "t",
                NodeKind::Trigger,
                json!({ "kind": TriggerKind::Manual }),
            ),
            node(
                "call",
                NodeKind::ToolCall,
                json!({ "slug": "svc.do", "args": { "flag": "=nodes.t.item.seeded" } }),
            ),
        ],
        edges: vec![edge("t", "call")],
        ..Default::default()
    };
    let trace = trace_of(&graph, None).await;

    let step = trace.steps_for("call")[0];
    let binding = step
        .bindings
        .iter()
        .find(|b| b.location == "args.flag")
        .expect("the binding is traced");
    assert!(!binding.is_null);
    assert_eq!(binding.value, json!(true));
    assert!(trace.null_bindings().is_empty());
}

#[tokio::test]
async fn capability_calls_are_folded_in_and_attributed_to_their_node() {
    let mocks = Arc::new(MockCaps::new().on_tool("svc.do", Respond::value(json!({ "ok": true }))));
    let trace = trace_of(&graph_with_a_null_binding(), Some(mocks)).await;

    let calls = trace.calls_from("call");
    assert_eq!(calls.len(), 1, "the tool node made one call");
    assert_eq!(calls[0].target, "svc.do");
    assert_eq!(
        calls[0].node_id.as_deref(),
        Some("call"),
        "a call must be attributed to the node that made it"
    );
}

#[tokio::test]
async fn a_failed_activation_is_recorded_with_its_error() {
    let graph = WorkflowGraph {
        name: "fails".to_string(),
        nodes: vec![
            node(
                "t",
                NodeKind::Trigger,
                json!({ "kind": TriggerKind::Manual }),
            ),
            // No slug: the tool node fails deterministically.
            node(
                "call",
                NodeKind::ToolCall,
                json!({ "on_error": "continue" }),
            ),
        ],
        edges: vec![edge("t", "call")],
        ..Default::default()
    };
    let trace = trace_of(&graph, None).await;

    let failed = trace.failed();
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].node_id, "call");
    assert!(
        failed[0].error.is_some(),
        "a failed step should carry why it failed"
    );
}

#[tokio::test]
async fn the_diagnosis_reports_what_a_green_outcome_hides() {
    let trace = trace_of(&graph_with_a_null_binding(), None).await;
    assert!(
        !trace.diagnosis.is_clean(),
        "a run with a null binding is not clean, however green its outcome"
    );
}

#[tokio::test]
async fn ran_distinguishes_a_node_that_executed_from_one_that_did_not() {
    let trace = trace_of(&graph_with_a_null_binding(), None).await;
    assert!(trace.ran("call"));
    assert!(!trace.ran("nonexistent"));
}

#[tokio::test]
async fn a_summary_reads_as_one_line() {
    let trace = trace_of(&graph_with_a_null_binding(), None).await;
    let summary = trace.summary();
    assert!(summary.contains("null bindings"), "got {summary}");
}

#[test]
fn a_trace_round_trips_through_json() {
    // The tool surface hands these back as JSON, so the shape has to survive it.
    let trace = RunTrace::default();
    let encoded = serde_json::to_string(&trace).expect("serialize");
    let decoded: RunTrace = serde_json::from_str(&encoded).expect("deserialize");
    assert_eq!(trace, decoded);
}
