//! Node execution: the [`NodeExecutor`] trait and per-kind implementations.
//!
//! Each [`crate::model::NodeKind`] maps to a `NodeExecutor`. Native control-flow
//! nodes live in [`control_flow`]; capability-backed nodes (which call the host
//! via [`crate::caps`]) live in [`integration`]. Implementations are stubbed in
//! this skeleton and completed in stages A2–A3 (see `docs/08-roadmap.md`).

pub mod control_flow;
pub mod integration;

use async_trait::async_trait;
use serde_json::Value;

use crate::caps::Capabilities;
use crate::error::Result;
use crate::model::Node;

/// The runtime context handed to a node when it executes.
pub struct NodeContext<'a> {
    /// The node being executed.
    pub node: &'a Node,
    /// The current run state (dynamic JSON).
    pub state: &'a Value,
    /// Host-provided capabilities.
    pub caps: &'a Capabilities,
}

/// The outcome of executing a single node.
#[derive(Debug, Clone)]
pub struct NodeOutput {
    /// The value this node contributes back to the run state.
    pub value: Value,
    /// For branching nodes, the output port to follow (e.g. `"true"`); `None`
    /// means the default `"main"` port.
    pub port: Option<String>,
}

impl NodeOutput {
    /// Builds an output on the default `"main"` port.
    #[must_use]
    pub fn main(value: Value) -> Self {
        Self { value, port: None }
    }

    /// Builds an output that routes to the named `port`.
    #[must_use]
    pub fn routed(value: Value, port: impl Into<String>) -> Self {
        Self {
            value,
            port: Some(port.into()),
        }
    }
}

/// Executes one node kind.
#[async_trait]
pub trait NodeExecutor: Send + Sync {
    /// Runs the node and returns its output (or a routing decision).
    async fn execute(&self, ctx: NodeContext<'_>) -> Result<NodeOutput>;
}
