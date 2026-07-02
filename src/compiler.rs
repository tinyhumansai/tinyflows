//! Compiles a validated [`WorkflowGraph`] into a runnable form.
//!
//! **Stage A1 target:** lower the graph onto a
//! `tinyagents::graph::CompiledGraph<serde_json::Value>`, built fresh per
//! definition (the `model_council` per-request pattern). Edges become
//! `add_edge` / `add_conditional_edges` (branch & switch) / `add_waiting_edge`
//! (merge), node kinds become graph nodes dispatched via [`crate::nodes`], and
//! `serde_json::Value` is the graph state for dynamic per-node I/O.
//! See `docs/04-execution-engine.md`.
//!
//! In this skeleton, [`compile`] validates the graph and returns an opaque
//! [`CompiledWorkflow`] handle; the tinyagents lowering lands in A1.

use crate::error::Result;
use crate::model::WorkflowGraph;
use crate::validate::validate;

/// A validated, compiled workflow ready to be run by [`crate::engine::run`].
///
/// Opaque by design: the internal tinyagents graph representation is added in
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
mod tests {
    use super::*;
    use crate::model::{Node, NodeKind};

    #[test]
    fn compiles_a_valid_graph() {
        let graph = WorkflowGraph {
            nodes: vec![Node {
                id: "t".to_string(),
                kind: NodeKind::Trigger,
                type_version: 1,
                name: "start".to_string(),
                config: serde_json::Value::Null,
                ports: Vec::new(),
                position: None,
            }],
            ..Default::default()
        };
        let compiled = compile(&graph).expect("compile");
        assert_eq!(compiled.graph.nodes.len(), 1);
    }

    #[test]
    fn rejects_an_invalid_graph() {
        let graph = WorkflowGraph::default();
        assert!(compile(&graph).is_err());
    }
}
