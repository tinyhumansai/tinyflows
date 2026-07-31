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

    /// Two declared inputs share the same name, so one would shadow the other
    /// in the `inputs` expression scope.
    #[error("duplicate workflow input name: {0}")]
    DuplicateInputName(String),

    /// A declared input's name is not a plain identifier, so `=inputs.<name>`
    /// could not address it without jq quoting.
    #[error(
        "invalid workflow input name {0:?} — names must match [A-Za-z_][A-Za-z0-9_]* so \
         `=inputs.<name>` can address them"
    )]
    InvalidInputName(String),

    /// A declared input's `default` does not satisfy its own declared type, so
    /// an omitted value would inject a wrongly-typed one.
    #[error("workflow input {name:?} has a default that is not of its declared type {expected}")]
    InputDefaultTypeMismatch {
        /// The offending input's name.
        name: String,
        /// The declared type's wire name.
        expected: &'static str,
    },

    /// A declared input is both `required` and has a `default`. The default
    /// always supplies a value, so the requirement could never fire — one of
    /// the two is a mistake.
    #[error("workflow input {0:?} is both required and has a default; a default makes it optional")]
    RequiredInputWithDefault(String),
}

impl ValidationError {
    /// A stable, machine-readable code for this error variant.
    ///
    /// Unlike the human-readable [`Display`](std::fmt::Display) form, this is a
    /// fixed `snake_case` identifier safe for a host to switch on or surface to
    /// an agent as a structured `code` field. It never changes for a given
    /// variant.
    pub fn code(&self) -> &'static str {
        match self {
            Self::MissingTrigger => "missing_trigger",
            Self::MultipleTriggers(_) => "multiple_triggers",
            Self::UnknownNode(_) => "unknown_node",
            Self::DuplicateNodeId(_) => "duplicate_node_id",
            Self::IllegalCycle(_) => "illegal_cycle",
            Self::InvalidNodeConfig { .. } => "invalid_node_config",
            Self::MissingErrorRoute(_) => "missing_error_route",
            Self::DuplicateEdge { .. } => "duplicate_edge",
            Self::InvalidOnError { .. } => "invalid_on_error",
            Self::SchemaVersionTooNew { .. } => "schema_version_too_new",
            Self::InvalidConditionRouting { .. } => "invalid_condition_routing",
            Self::DuplicateInputName(_) => "duplicate_input_name",
            Self::InvalidInputName(_) => "invalid_input_name",
            Self::InputDefaultTypeMismatch { .. } => "input_default_type_mismatch",
            Self::RequiredInputWithDefault(_) => "required_input_with_default",
        }
    }

    /// The node id this error is anchored to, when it is node-specific.
    ///
    /// Returns `None` for graph-wide errors (`MissingTrigger`,
    /// `SchemaVersionTooNew`), for `MultipleTriggers` (which carries many ids in
    /// its payload rather than a single anchor), and for the declared-input
    /// errors (anchored to an input, not a node — see [`Self::input_name`]).
    /// Lets a host attach the error to the right node in a structured
    /// validation report.
    pub fn node_id(&self) -> Option<&str> {
        match self {
            Self::UnknownNode(id)
            | Self::DuplicateNodeId(id)
            | Self::IllegalCycle(id)
            | Self::MissingErrorRoute(id) => Some(id),
            Self::InvalidNodeConfig { node, .. }
            | Self::InvalidOnError { node, .. }
            | Self::InvalidConditionRouting { node, .. } => Some(node),
            Self::DuplicateEdge { from_node, .. } => Some(from_node),
            Self::MissingTrigger
            | Self::MultipleTriggers(_)
            | Self::SchemaVersionTooNew { .. }
            | Self::DuplicateInputName(_)
            | Self::InvalidInputName(_)
            | Self::InputDefaultTypeMismatch { .. }
            | Self::RequiredInputWithDefault(_) => None,
        }
    }

    /// The declared input this error is anchored to, when it is input-specific.
    ///
    /// The counterpart to [`Self::node_id`]: lets a host attach the error to the
    /// right field of an inputs editor. Returns `None` for every node-anchored
    /// and graph-wide error.
    pub fn input_name(&self) -> Option<&str> {
        match self {
            Self::DuplicateInputName(name)
            | Self::InvalidInputName(name)
            | Self::RequiredInputWithDefault(name) => Some(name),
            Self::InputDefaultTypeMismatch { name, .. } => Some(name),
            _ => None,
        }
    }
}

/// Errors produced while compiling or running a workflow.
#[derive(Debug, Error)]
pub enum EngineError {
    /// The workflow graph failed validation before compilation.
    #[error("validation failed: {0}")]
    Validation(#[from] ValidationError),

    /// The values supplied for the workflow's declared inputs were rejected.
    ///
    /// Raised before any node executes and before the run is recorded, so a
    /// caller that gets this can be certain nothing ran.
    #[error("input error: {0}")]
    Input(#[from] crate::model::InputError),

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
    fn declared_input_validation_error_display_and_anchors() {
        let dup = ValidationError::DuplicateInputName("repo".to_string());
        assert_eq!(dup.to_string(), "duplicate workflow input name: repo");
        assert_eq!(dup.code(), "duplicate_input_name");
        assert_eq!(dup.input_name(), Some("repo"));
        assert_eq!(dup.node_id(), None);

        assert_eq!(
            ValidationError::InvalidInputName("repo-url".to_string()).to_string(),
            "invalid workflow input name \"repo-url\" — names must match \
             [A-Za-z_][A-Za-z0-9_]* so `=inputs.<name>` can address them"
        );
        assert_eq!(
            ValidationError::InputDefaultTypeMismatch {
                name: "depth".to_string(),
                expected: "number",
            }
            .to_string(),
            "workflow input \"depth\" has a default that is not of its declared type number"
        );
        assert_eq!(
            ValidationError::RequiredInputWithDefault("repo".to_string()).to_string(),
            "workflow input \"repo\" is both required and has a default; \
             a default makes it optional"
        );

        // Node-anchored errors are not input-anchored, and vice versa.
        assert_eq!(
            ValidationError::UnknownNode("ghost".to_string()).input_name(),
            None
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
        assert_eq!(
            EngineError::Input(crate::model::InputError::Missing("repo".to_string())).to_string(),
            "input error: workflow input \"repo\" is required but was not supplied"
        );
    }

    #[test]
    fn input_error_lifts_into_engine_error() {
        let engine: EngineError = crate::model::InputError::Unknown("reop".to_string()).into();
        match engine {
            EngineError::Input(inner) => assert_eq!(inner.input_name(), "reop"),
            other => panic!("expected lifted input error, got {other:?}"),
        }
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
