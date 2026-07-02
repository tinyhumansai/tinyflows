//! Compiles a [`WorkflowGraph`] into a runnable handle.
//!
//! [`compile`] runs structural validation over the graph (via
//! [`crate::validate::validate`]) and, on success, returns an opaque
//! [`CompiledWorkflow`] holding the validated graph. Compilation is therefore
//! validation plus handle creation — it performs no lowering itself.
//!
//! The graph is lowered onto a fresh `tinyagents` state graph once per run,
//! inside [`crate::engine::run`], which captures that run's host capabilities.
//! Building the state graph per run keeps compilation independent of any
//! particular set of capabilities.

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
