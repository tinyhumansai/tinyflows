//! Core library surface for **tinyflows** — a Rust-native workflow
//! engine.
//!
//! tinyflows models an automation as a [`model::WorkflowGraph`]: a directed graph
//! of typed [`model::Node`]s connected by [`model::Edge`]s. A [`compiler::compile`]
//! step validates the graph and (from stage A1) lowers it onto the
//! in-crate [`graph`](crate::graph) state-graph runtime, which
//! the [`engine::run`] entry point drives.
//!
//! The crate is deliberately **host-agnostic**: anything that touches the outside
//! world — LLM calls, integration tools, HTTP, code execution, persistence — is
//! expressed through the [`caps`] capability traits that the embedding
//! application implements.
//!
//! ```
//! assert_eq!(tinyflows::product_name(), "tinyflows");
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

/// Reading the expression bindings a graph declares — which node an `=`
/// expression reads from, and whether it reads as prose.
pub mod bindings;
pub mod caps;
pub mod catalog;
/// Topologies this engine's fan-in lowering cannot execute safely, refused
/// before a run rather than surfacing as dropped data or a hung barrier.
pub mod compat;
pub mod compiler;
pub mod data;
/// Reading a run's steps for the failures a green outcome hides: null bindings,
/// empty agent prompts, errors an `on_error` policy swallowed, and nodes a
/// branch routed past.
pub mod diagnostics;
pub mod engine;
pub mod error;
/// Bounding what a run hands back — durable records and tool replies alike —
/// so one large item cannot bloat every future read of it.
pub mod evidence;
pub mod expr;
/// Authoring gates: what is *guaranteed* wrong with a graph, caught before a
/// write lands rather than as a silent null at run time.
pub mod gates;
pub mod graph;
pub mod graph_ops;
/// The engine's execution-gating hook: unlike a `RunObserver`, what a
/// `StepInterceptor` returns is obeyed. What breakpoints and output overrides
/// are built on.
pub mod interception;
// Only the file-backed store, the process-backed capabilities, and the
// testkit's debug sessions need unique scratch names, and all three are
// optional.
#[cfg(any(test, feature = "store", feature = "host-caps", feature = "testkit"))]
mod ids;
pub mod migrate;
pub mod model;
pub mod nodes;
pub mod observability;
/// Stored workflows and their run history: the durable model around a graph,
/// and a file-backed store for it. Behind the `store` feature.
#[cfg(any(test, feature = "store"))]
pub mod store;
/// Testing, mocking, and live debugging for workflows: programmable capability
/// doubles, a structured run trace, breakpoints, and an agent-facing tool
/// surface over all of it. Behind the `testkit` feature.
#[cfg(any(test, feature = "testkit"))]
pub mod testkit;
pub mod transcript;
pub mod validate;
// Resolving an author-supplied working directory against the run's workspace —
// the containment rule a shell step's `args.cwd` already obeyed, shared with the
// `agent` and `sub_workflow` nodes so there is exactly one of them.
/// Render workflow structure to PNG or JPEG files for visual debugging.
#[cfg(feature = "graph-debug")]
pub mod visualization;
mod workdir;

/// The crate name published to crates.io.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

/// The crate version from `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Returns the user-facing product name.
pub fn product_name() -> &'static str {
    CRATE_NAME
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
