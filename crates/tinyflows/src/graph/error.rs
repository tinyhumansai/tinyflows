//! Error type for the in-crate state-graph runtime.
//!
//! The runtime under [`crate::graph`] used to live in the external `tinyagents`
//! crate, whose crate-wide error type carried variants for surfaces tinyflows
//! never had (model providers, tools, blueprint parsing). [`GraphError`] keeps
//! only the graph-execution variants the runtime actually raises; the engine
//! maps them onto [`crate::error::EngineError`] at the boundary.

use thiserror::Error;

/// Convenience alias for a graph-runtime result.
pub type Result<T> = std::result::Result<T, GraphError>;

/// Failures raised while building, compiling, or running a state graph.
#[derive(Debug, Error)]
pub enum GraphError {
    /// A graph was compiled or run without a configured `START` edge, so there
    /// is no entry node to begin execution from.
    #[error("graph start node is not configured")]
    MissingStart,

    /// An edge, route, or run referenced a node name that is not present in the
    /// graph. The payload is the missing node name.
    #[error("node `{0}` does not exist")]
    MissingNode(String),

    /// An edge declares a destination node that does not exist. The payload is
    /// the missing target name.
    #[error("edge points to missing node `{0}`")]
    MissingEdgeTarget(String),

    /// A conditional router returned a `route` label that is not wired to any
    /// destination from `node`.
    #[error("conditional route `{route}` from node `{node}` does not exist")]
    MissingRoute {
        /// The node whose router produced the label.
        node: String,
        /// The unroutable label.
        route: String,
    },

    /// Graph execution performed more super-steps than the configured recursion
    /// limit allows (typically an unintended cycle). The payload is the limit
    /// that was hit.
    #[error("graph exceeded the recursion limit of {0} steps")]
    RecursionLimit(usize),

    /// A nested run would exceed the configured maximum recursion depth. The
    /// payload is the `max_depth` cap that was reached.
    #[error("sub-agent recursion exceeded the maximum depth of {0}")]
    SubAgentDepth(usize),

    /// A single graph node was activated more times within one run than the
    /// [`RecursionPolicy`](crate::graph::RecursionPolicy)'s
    /// `max_visits_per_node` allows (an unbounded node-loop).
    #[error("node `{node}` exceeded its visit limit of {limit}")]
    NodeVisitLimit {
        /// The node that was over-visited.
        node: String,
        /// The configured per-node visit cap.
        limit: usize,
    },

    /// Input failed validation before the graph ran. The payload describes the
    /// problem.
    #[error("validation error: {0}")]
    Validation(String),

    /// A node exceeded its configured wall-clock budget.
    #[error("run timed out: {0}")]
    Timeout(String),

    /// The run was cancelled before completion.
    #[error("run cancelled")]
    Cancelled,

    /// Generic graph runtime error.
    #[error("graph error: {0}")]
    Graph(String),

    /// Execution was interrupted (human-in-the-loop / external approval).
    #[error("graph interrupted at node `{node}`: {message}")]
    Interrupted {
        /// The node that paused the run.
        node: String,
        /// The interrupt's human-readable payload.
        message: String,
    },

    /// Two or more concurrent branches in a single superstep wrote the same
    /// non-aggregate channel, so the merge cannot pick a deterministic winner.
    /// The payload describes the offending channel.
    #[error("invalid concurrent update: {0}")]
    InvalidConcurrentUpdate(String),

    /// A checkpoint could not be written, read, or located.
    #[error("checkpoint error: {0}")]
    Checkpoint(String),

    /// Resume was requested but checkpointing was not configured, or no
    /// checkpoint was found.
    #[error("cannot resume: {0}")]
    Resume(String),

    /// A `serde_json` (de)serialization failure, converted via `?` wherever
    /// checkpoints and observations are read or written.
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}
