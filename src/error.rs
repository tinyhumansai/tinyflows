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

    /// A node sets `on_error: "route"` but has no outgoing edge on its `error`
    /// port, so the routed error item would have nowhere to go.
    #[error("node {0} has on_error=\"route\" but no outgoing edge on its `error` port")]
    MissingErrorRoute(String),

    /// Two edges are identical (same source node/port and destination
    /// node/port), which is redundant and almost always an authoring mistake.
    #[error("duplicate edge: {from_node}.{from_port} -> {to_node}.{to_port}")]
    DuplicateEdge {
        /// Source node id.
        from_node: String,
        /// Source port name.
        from_port: String,
        /// Destination node id.
        to_node: String,
        /// Destination port name.
        to_port: String,
    },

    /// A node's `on_error` policy is not one of `stop`, `continue`, or `route`.
    #[error("node {node} has unknown on_error value: {value:?}")]
    InvalidOnError {
        /// The offending node id.
        node: String,
        /// The unrecognized `on_error` value.
        value: String,
    },

    /// A persisted graph declares a `schema_version` newer than this crate
    /// understands; it cannot be safely migrated (and must not be downgraded).
    #[error(
        "schema_version {found} is newer than this crate supports (max {supported}); \
         upgrade tinyflows to load this graph"
    )]
    SchemaVersionTooNew {
        /// The version found in the persisted document.
        found: u32,
        /// The highest schema version this crate understands.
        supported: u32,
    },

    /// A `condition` node has an outgoing edge whose `from_port` is not one of
    /// its two declared branch ports (`"true"` / `"false"`).
    ///
    /// Routing is keyed EXCLUSIVELY on `from_port` (see `engine::outgoing_by_port`
    /// / `handler_routing`) — `to_port` is never consulted to decide which
    /// successor fires. A condition node authored with the branch label on
    /// `to_port` instead (e.g. `{from_port:"main", to_port:"true"}` and
    /// `{from_port:"main", to_port:"false"}`) puts both edges in the SAME
    /// `from_port` group, which `handler_routing` classifies as a parallel
    /// `FanOut` — silently driving BOTH branches unconditionally instead of
    /// gating on the condition's actual result. This is a HARD authoring
    /// mistake, not a runtime data issue, so it is rejected here rather than
    /// left as a silent no-op condition.
    #[error(
        "condition node {node} has an outgoing edge with from_port {from_port:?} — condition \
         edges must emit on from_port \"true\" or \"false\" (the branch label belongs on \
         from_port, not to_port; routing is keyed exclusively on from_port)"
    )]
    InvalidConditionRouting {
        /// The offending condition node's id.
        node: String,
        /// The edge's actual (invalid) `from_port` value.
        from_port: String,
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
        assert_eq!(
            ValidationError::MissingErrorRoute("n1".to_string()).to_string(),
            "node n1 has on_error=\"route\" but no outgoing edge on its `error` port"
        );
        assert_eq!(
            ValidationError::DuplicateEdge {
                from_node: "a".to_string(),
                from_port: "main".to_string(),
                to_node: "b".to_string(),
                to_port: "main".to_string(),
            }
            .to_string(),
            "duplicate edge: a.main -> b.main"
        );
        assert_eq!(
            ValidationError::InvalidOnError {
                node: "n1".to_string(),
                value: "explode".to_string(),
            }
            .to_string(),
            "node n1 has unknown on_error value: \"explode\""
        );
        assert_eq!(
            ValidationError::SchemaVersionTooNew {
                found: 5,
                supported: 1,
            }
            .to_string(),
            "schema_version 5 is newer than this crate supports (max 1); \
             upgrade tinyflows to load this graph"
        );
        assert_eq!(
            ValidationError::InvalidConditionRouting {
                node: "gate".to_string(),
                from_port: "main".to_string(),
            }
            .to_string(),
            "condition node gate has an outgoing edge with from_port \"main\" — condition \
             edges must emit on from_port \"true\" or \"false\" (the branch label belongs on \
             from_port, not to_port; routing is keyed exclusively on from_port)"
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
