//! Error types shared across validation, compilation, and execution.

use thiserror::Error;

/// Errors produced while validating a [`crate::model::WorkflowGraph`].
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ValidationError {
    /// The graph has no trigger node (exactly one is required).
    #[error("workflow has no trigger node")]
    MissingTrigger,

    /// The graph has more than one trigger node.
    #[error("workflow has multiple trigger nodes: {0:?}")]
    MultipleTriggers(Vec<String>),

    /// An edge references a node id that does not exist.
    #[error("edge references unknown node id: {0}")]
    UnknownNode(String),

    /// Two nodes share the same id.
    #[error("duplicate node id: {0}")]
    DuplicateNodeId(String),

    /// The graph contains a cycle through nodes that may not participate in loops.
    #[error("illegal cycle detected involving node: {0}")]
    IllegalCycle(String),

    /// A node's configuration is invalid for its kind.
    #[error("invalid config for node {node}: {reason}")]
    InvalidNodeConfig {
        /// The offending node id.
        node: String,
        /// Why the configuration is invalid.
        reason: String,
    },
}

/// Errors produced while compiling or running a workflow.
#[derive(Debug, Error)]
pub enum EngineError {
    /// The workflow graph failed validation before compilation.
    #[error("validation failed: {0}")]
    Validation(#[from] ValidationError),

    /// A feature required by the graph is not yet implemented in this stage.
    #[error("not yet implemented: {0}")]
    Unimplemented(&'static str),

    /// A host capability call failed at runtime.
    #[error("capability error: {0}")]
    Capability(String),
}

/// Convenience result alias for compile/run operations.
pub type Result<T> = std::result::Result<T, EngineError>;
