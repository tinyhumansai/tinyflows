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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_error_display() {
        assert_eq!(
            ValidationError::MissingTrigger.to_string(),
            "workflow has no trigger node"
        );
        assert_eq!(
            ValidationError::MultipleTriggers(vec!["t1".to_string(), "t2".to_string()]).to_string(),
            "workflow has multiple trigger nodes: [\"t1\", \"t2\"]"
        );
        assert_eq!(
            ValidationError::UnknownNode("ghost".to_string()).to_string(),
            "edge references unknown node id: ghost"
        );
        assert_eq!(
            ValidationError::DuplicateNodeId("dup".to_string()).to_string(),
            "duplicate node id: dup"
        );
        assert_eq!(
            ValidationError::IllegalCycle("loop".to_string()).to_string(),
            "illegal cycle detected involving node: loop"
        );
        assert_eq!(
            ValidationError::InvalidNodeConfig {
                node: "n1".to_string(),
                reason: "missing url".to_string(),
            }
            .to_string(),
            "invalid config for node n1: missing url"
        );
    }

    #[test]
    fn engine_error_display() {
        assert_eq!(
            EngineError::Unimplemented("checkpoint replay").to_string(),
            "not yet implemented: checkpoint replay"
        );
        assert_eq!(
            EngineError::Capability("http timed out".to_string()).to_string(),
            "capability error: http timed out"
        );
        assert_eq!(
            EngineError::Validation(ValidationError::MissingTrigger).to_string(),
            "validation failed: workflow has no trigger node"
        );
    }

    #[test]
    fn validation_error_converts_into_engine_error() {
        let engine: EngineError = ValidationError::MissingTrigger.into();
        assert!(matches!(
            engine,
            EngineError::Validation(ValidationError::MissingTrigger)
        ));
    }

    #[test]
    fn question_mark_operator_lifts_validation_error() {
        fn inner() -> Result<()> {
            Err(ValidationError::DuplicateNodeId("dup".to_string()))?;
            Ok(())
        }
        match inner() {
            Err(EngineError::Validation(ValidationError::DuplicateNodeId(id))) => {
                assert_eq!(id, "dup");
            }
            other => panic!("expected lifted validation error, got {other:?}"),
        }
    }

    #[test]
    fn validation_error_is_comparable_and_cloneable() {
        let err = ValidationError::UnknownNode("x".to_string());
        assert_eq!(err.clone(), err);
        assert_ne!(err, ValidationError::MissingTrigger);
    }
}
