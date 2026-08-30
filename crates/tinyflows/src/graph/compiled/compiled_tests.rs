//! Unit tests for the superstep executor: sequential and parallel runs,
//! reducer fan-in ordering, conditional/command routing, checkpoint
//! persistence, interrupt/resume, and recursion-limit enforcement.

use super::*;
use crate::graph::builder::{GraphBuilder, GraphDefaults, NodeContext, Route};
use crate::graph::checkpoint::{Checkpointer, InMemoryCheckpointer};
use crate::graph::command::{Command, Interrupt, NodeResult, Send};
use crate::graph::ids::ExecutionStatus;
use crate::graph::reducer::ClosureStateReducer;
use crate::graph::stream::{CollectingSink, GraphEvent};
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering};
use std::time::Duration;

#[derive(Clone, Debug, PartialEq)]
struct Counter {
    value: i32,
    log: Vec<String>,
}

/// Builds a graph whose nodes return partial `i32` updates merged by a custom
/// reducer that adds to `value` and records a log entry.
fn adding_graph() -> CompiledGraph<Counter, i32> {
    GraphBuilder::<Counter, i32>::new()
        .set_reducer(ClosureStateReducer::new(|mut s: Counter, u: i32| {
            s.value += u;
            s.log.push(format!("+{u}"));
            Ok(s)
        }))
        .add_node("inc", |_s: Counter, _c: NodeContext| async move {
            Ok(NodeResult::Update(1))
        })
        .add_node("double", |s: Counter, _c: NodeContext| async move {
            Ok(NodeResult::Update(s.value))
        })
        .set_entry("inc")
        .add_edge("inc", "double")
        .set_finish("double")
        .compile()
        .unwrap()
}

include!("compiled_tests/compiled_tests_part_01_tests.rs");
include!("compiled_tests/compiled_tests_part_02_tests.rs");
include!("compiled_tests/compiled_tests_part_03_tests.rs");
include!("compiled_tests/compiled_tests_part_04_tests.rs");
include!("compiled_tests/compiled_tests_part_05_tests.rs");
include!("compiled_tests/compiled_tests_part_06_tests.rs");
include!("compiled_tests/compiled_tests_part_07_tests.rs");
include!("compiled_tests/compiled_tests_part_08_tests.rs");
