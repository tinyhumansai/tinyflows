//! The failure vocabulary every workflow surface reports through.
//!
//! Kept deliberately wide rather than collapsed into one string: the CLI, the
//! MCP server, and the TUI all branch on these, and an operator's next step
//! differs for each.

use std::path::PathBuf;

use super::run::RunId;
use super::workflow::WorkflowId;

/// What can go wrong reading, writing, or running a workflow.
#[derive(Debug, thiserror::Error)]
pub enum WorkflowError {
    /// No workflow with that id is known to the store.
    #[error("no workflow with id '{0}'")]
    NotFound(WorkflowId),

    /// No run with that id is known to the store.
    #[error("no run with id '{0}'")]
    RunNotFound(RunId),

    /// The graph did not pass the engine's validation. Carries every failure,
    /// not just the first, so one round-trip tells an author everything.
    #[error("workflow '{id}' is invalid: {}", .messages.join("; "))]
    Invalid {
        /// The workflow that failed validation.
        id: WorkflowId,
        /// One message per validation failure.
        messages: Vec<String>,
    },

    /// A document could not be read or parsed.
    #[error("{0}")]
    Malformed(String),

    /// A visible definition belongs to a read-only, lower-precedence layer.
    #[error(
        "workflow '{id}' comes from repository default {path}; save it to a writable layer before deleting it"
    )]
    ReadOnlyDefinition {
        /// The workflow the operator tried to delete.
        id: WorkflowId,
        /// The lower-precedence definition that remains untouched.
        path: PathBuf,
    },

    /// The filesystem refused an operation.
    #[error("{path}: {source}")]
    Io {
        /// The path being operated on.
        path: PathBuf,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },

    /// The engine refused to compile or run the graph.
    #[error("{0}")]
    Engine(String),

    /// A dispatch to a harness ran out of time before it replied.
    ///
    /// Kept apart from the three below because the operator's next step differs
    /// for each: a timeout is worth retrying, an abort was deliberate, a harness
    /// error wants reading, and an unreachable harness wants configuring.
    #[error("the harness did not respond in time")]
    DispatchTimeout,

    /// A dispatch was aborted before it replied.
    #[error("the turn was aborted")]
    DispatchAborted,

    /// The harness ran and reported a failure of its own.
    #[error("harness: {0}")]
    Harness(String),

    /// The dispatch never reached a harness — no transport, no worker, or the
    /// waiter went away.
    #[error("could not reach a harness: {0}")]
    Unreachable(String),
}
