//! Unit tests for the graph durable observability layer: journaling sinks,
//! offset-addressable replay, status-store lifecycle, store-backed journals, and
//! namespaced subgraph observations.

use super::*;
use crate::graph::builder::{GraphBuilder, NodeContext};
use crate::graph::command::NodeResult;
use crate::graph::compiled::CompiledGraph;
use crate::graph::error::GraphError;
use crate::graph::ids::{ExecutionStatus, GraphId, NodeId, RunId};
use crate::graph::stream::{CollectingSink, GraphEvent, GraphEventSink};
use std::sync::Arc;

/// A two-node line graph over `i32` with overwrite semantics: `a -> b`.
fn line_graph() -> CompiledGraph<i32, i32> {
    GraphBuilder::<i32, i32>::overwrite()
        .add_node("a", |_s, _c: NodeContext| async move {
            Ok(NodeResult::Update(1))
        })
        .add_node("b", |s, _c: NodeContext| async move {
            Ok(NodeResult::Update(s + 1))
        })
        .set_entry("a")
        .add_edge("a", "b")
        .set_finish("b")
        .compile()
        .unwrap()
}

/// A single-node graph whose node always errors.
fn failing_graph() -> CompiledGraph<i32, i32> {
    GraphBuilder::<i32, i32>::overwrite()
        .add_node("boom", |_s, _c: NodeContext| async move {
            Err::<NodeResult<i32>, _>(GraphError::Validation("boom".to_string()))
        })
        .set_entry("boom")
        .set_finish("boom")
        .compile()
        .unwrap()
}

fn graph_obs(run: &str, offset: u64, step: usize, event: GraphEvent) -> GraphObservation {
    GraphObservation {
        event_id: crate::graph::ids::EventId::new(format!("g-evt-{offset}")),
        run_id: RunId::new(run),
        root_run_id: RunId::new(run),
        parent_run_id: None,
        thread_id: None,
        graph_id: GraphId::new("graph-latency"),
        checkpoint_id: None,
        namespace: Vec::new(),
        step,
        offset,
        ts_ms: 1_000 + offset,
        event,
    }
}

include!("observability_tests/observability_tests_part_01_tests.rs");
include!("observability_tests/observability_tests_part_02_tests.rs");
