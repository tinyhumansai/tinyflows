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

    /// Two entries in the graph's agent registry share an id, so an
    /// `agent_ref` naming it would resolve ambiguously.
    #[error("duplicate agent definition id: {0}")]
    DuplicateAgentId(String),

    /// An entry in the graph's agent registry is malformed.
    #[error("invalid agent definition {agent}: {reason}")]
    InvalidAgentDefinition {
        /// The offending definition's id, empty when the id itself is the
        /// problem.
        agent: String,
        /// Why the definition is invalid.
        reason: String,
    },

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
            Self::DuplicateAgentId(_) => "duplicate_agent_id",
            Self::InvalidAgentDefinition { .. } => "invalid_agent_definition",
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
    /// Also `None` for the agent-registry errors, which are anchored to an agent
    /// definition rather than to any one node that references it.
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
            | Self::RequiredInputWithDefault(_)
            | Self::DuplicateAgentId(_)
            | Self::InvalidAgentDefinition { .. } => None,
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

    /// A `loop` node reached its `max_iterations` cap while its `on_exceeded`
    /// policy was `"error"` (the default).
    ///
    /// Distinct from the graph-wide `recursion_limit`, which bounds total
    /// super-steps and cannot say *which* loop ran away. A node that would
    /// rather finish with partial results than fail sets
    /// `on_exceeded: "continue"` and exits on its `done` port instead.
    #[error("loop node {node} exceeded its maximum of {limit} iterations")]
    LoopLimit {
        /// The `loop` node that hit its cap.
        node: String,
        /// The `max_iterations` value it was configured with.
        limit: u64,
    },
}

/// Convenience result alias for compile/run operations.
pub type Result<T> = std::result::Result<T, EngineError>;

#[cfg(test)]
#[path = "error_tests.rs"]
mod tests;
