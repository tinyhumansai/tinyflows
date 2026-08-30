//! Compiles a [`WorkflowGraph`] into a runnable handle.
//!
//! [`compile`] runs structural validation over the graph (via
//! [`crate::validate::validate`]) and, on success, returns an opaque
//! [`CompiledWorkflow`] holding the validated graph. Compilation is therefore
//! validation plus handle creation — it performs no lowering itself.
//!
//! The graph is lowered onto a fresh `crate::graph` state graph once per run,
//! inside [`crate::engine::run`], which captures that run's host capabilities.
//! Building the state graph per run keeps compilation independent of any
//! particular set of capabilities.

use crate::error::Result;
use crate::model::WorkflowGraph;
use crate::validate::validate;

/// A validated, compiled workflow ready to be run by [`crate::engine::run`].
///
/// Opaque by design: the internal graph representation is added in
/// stage A1 without changing this public handle.
#[derive(Debug, Clone)]
pub struct CompiledWorkflow {
    /// The validated source graph.
    pub graph: WorkflowGraph,
}

/// Validates and compiles a workflow graph.
///
/// # Errors
/// Returns a validation error if the graph is structurally invalid.
pub fn compile(graph: &WorkflowGraph) -> Result<CompiledWorkflow> {
    validate(graph)?;
    Ok(CompiledWorkflow {
        graph: graph.clone(),
    })
}

#[cfg(test)]
#[path = "compiler_tests.rs"]
mod tests;
