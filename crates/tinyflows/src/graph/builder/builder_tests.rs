//! Unit tests for the graph builder and its compile contract: reducer
//! requirement, START/END validation, missing-node/route detection, and the
//! rejection of nodes that mix command routing with static/conditional edges.

use super::*;
use crate::graph::command::NodeResult;
use crate::graph::error::GraphError;

type S = i32;

#[test]
fn compile_requires_reducer() {
    let err = GraphBuilder::<S, S>::new()
        .add_node("a", |s: S, _c: NodeContext| async move {
            Ok(NodeResult::Update(s))
        })
        .set_entry("a")
        .compile()
        .unwrap_err();
    assert!(matches!(err, GraphError::Validation(_)));
}

#[test]
fn compile_requires_start() {
    let err = GraphBuilder::<S, S>::overwrite()
        .add_node("a", |s: S, _c: NodeContext| async move {
            Ok(NodeResult::Update(s))
        })
        .compile()
        .unwrap_err();
    assert!(matches!(err, GraphError::MissingStart));
}

#[test]
fn compile_rejects_missing_edge_target() {
    let err = GraphBuilder::<S, S>::overwrite()
        .add_node("a", |s: S, _c: NodeContext| async move {
            Ok(NodeResult::Update(s))
        })
        .set_entry("a")
        .add_edge("a", "missing")
        .compile()
        .unwrap_err();
    assert!(matches!(err, GraphError::MissingNode(n) if n == "missing"));
}

#[test]
fn compile_rejects_command_routing_with_edges() {
    let err = GraphBuilder::<S, S>::overwrite()
        .add_node("a", |s: S, _c: NodeContext| async move {
            Ok(NodeResult::Update(s))
        })
        .add_node("b", |s: S, _c: NodeContext| async move {
            Ok(NodeResult::Update(s))
        })
        .set_entry("a")
        .add_edge("a", "b")
        .set_finish("b")
        .mark_command_routing("a")
        .compile()
        .unwrap_err();
    assert!(matches!(err, GraphError::Validation(_)));
}

#[test]
fn compile_rejects_static_and_conditional_on_same_node() {
    let err = GraphBuilder::<S, S>::overwrite()
        .add_node("a", |s: S, _c: NodeContext| async move {
            Ok(NodeResult::Update(s))
        })
        .add_node("b", |s: S, _c: NodeContext| async move {
            Ok(NodeResult::Update(s))
        })
        .set_entry("a")
        .add_edge("a", "b")
        .add_conditional_edges("a", |_s: &S| "x".to_string(), [("x", "b")])
        .set_finish("b")
        .compile()
        .unwrap_err();
    assert!(matches!(err, GraphError::Validation(_)));
}

#[test]
fn compile_succeeds_for_valid_graph() {
    let compiled = GraphBuilder::<S, S>::overwrite()
        .add_node("a", |s: S, _c: NodeContext| async move {
            Ok(NodeResult::Update(s))
        })
        .add_node("b", |s: S, _c: NodeContext| async move {
            Ok(NodeResult::Update(s))
        })
        .set_entry("a")
        .add_edge("a", "b")
        .set_finish("b")
        .compile();
    assert!(compiled.is_ok());
}

fn passthrough_builder() -> GraphBuilder<S, S> {
    GraphBuilder::<S, S>::overwrite()
        .add_node("a", |s: S, _c: NodeContext| async move {
            Ok(NodeResult::Update(s))
        })
        .add_node("b", |s: S, _c: NodeContext| async move {
            Ok(NodeResult::Update(s))
        })
}

#[test]
fn compile_rejects_start_routing_directly_to_end() {
    let err = GraphBuilder::<S, S>::overwrite()
        .add_edge(START, END)
        .compile()
        .unwrap_err();

    assert!(matches!(err, GraphError::Validation(message) if message.contains("directly")));
}

#[test]
fn compile_rejects_start_as_an_edge_target() {
    let err = passthrough_builder()
        .set_entry("a")
        .add_edge("a", START)
        .compile()
        .unwrap_err();

    assert!(matches!(err, GraphError::Validation(message) if message.contains("target")));
}

#[test]
fn compile_rejects_end_as_an_edge_source() {
    let err = passthrough_builder()
        .set_entry("a")
        .add_edge(END, "b")
        .compile()
        .unwrap_err();

    assert!(matches!(err, GraphError::Validation(message) if message.contains("source")));
}

#[test]
fn compile_rejects_unknown_conditional_source_and_target() {
    let source_err = passthrough_builder()
        .set_entry("a")
        .add_conditional_edges("missing", |_s: &S| "yes", [("yes", "b")])
        .compile()
        .unwrap_err();
    assert!(matches!(source_err, GraphError::MissingNode(node) if node == "missing"));

    let target_err = passthrough_builder()
        .set_entry("a")
        .add_conditional_edges("a", |_s: &S| "yes", [("yes", "missing")])
        .compile()
        .unwrap_err();
    assert!(matches!(target_err, GraphError::MissingNode(node) if node == "missing"));
}

#[test]
fn compile_rejects_unknown_waiting_edge_endpoints() {
    let source_err = passthrough_builder()
        .set_entry("a")
        .add_waiting_edge("missing", "b")
        .compile()
        .unwrap_err();
    assert!(matches!(source_err, GraphError::MissingNode(node) if node == "missing"));

    let target_err = passthrough_builder()
        .set_entry("a")
        .add_waiting_edge("a", "missing")
        .compile()
        .unwrap_err();
    assert!(matches!(target_err, GraphError::MissingNode(node) if node == "missing"));
}

#[test]
fn compile_rejects_unknown_command_node() {
    let err = passthrough_builder()
        .set_entry("a")
        .set_finish("a")
        .mark_command_routing("missing")
        .compile()
        .unwrap_err();

    assert!(matches!(err, GraphError::MissingNode(node) if node == "missing"));
}
