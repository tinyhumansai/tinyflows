//! Drives a [`CompiledWorkflow`] to completion.
//!
//! **Stage A1/A3 target:** execute the compiled tinyagents graph, dispatching
//! each node through its [`crate::nodes::NodeExecutor`] with the run state and
//! host [`Capabilities`]; support mid-run interrupt/resume for human-in-the-loop
//! approval steps. See `docs/04-execution-engine.md`.
//!
//! In this skeleton [`run`] returns [`crate::error::EngineError::Unimplemented`].

use serde_json::Value;

use crate::caps::Capabilities;
use crate::compiler::CompiledWorkflow;
use crate::error::{EngineError, Result};

/// The result of a completed workflow run.
#[derive(Debug, Clone)]
pub struct RunOutcome {
    /// The final run state after the terminal node(s) completed.
    pub output: Value,
}

/// Executes a compiled workflow with the given trigger `input` and host
/// `capabilities`.
///
/// # Errors
/// Returns an [`EngineError`] if execution fails. In this skeleton it always
/// returns [`EngineError::Unimplemented`] until the A1 lowering lands.
pub async fn run(
    workflow: &CompiledWorkflow,
    input: Value,
    capabilities: &Capabilities,
) -> Result<RunOutcome> {
    // Touch the arguments so the signature is stable and warning-free before the
    // real driver lands in A1.
    let _ = (&workflow.graph, &input, &capabilities.llm);
    Err(EngineError::Unimplemented("engine run (stage A1)"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::mock::mock_capabilities;
    use crate::compiler::compile;
    use crate::model::{Node, NodeKind, WorkflowGraph};

    #[tokio::test]
    async fn run_is_unimplemented_for_now() {
        let graph = WorkflowGraph {
            nodes: vec![Node {
                id: "t".to_string(),
                kind: NodeKind::Trigger,
                name: "start".to_string(),
                config: serde_json::Value::Null,
                ports: Vec::new(),
                position: None,
            }],
            ..Default::default()
        };
        let compiled = compile(&graph).expect("compile");
        let caps = mock_capabilities();
        let result = run(&compiled, serde_json::Value::Null, &caps).await;
        assert!(matches!(result, Err(EngineError::Unimplemented(_))));
    }
}
