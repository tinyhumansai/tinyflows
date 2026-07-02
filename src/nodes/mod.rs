//! Node execution: the [`NodeExecutor`] trait and per-kind implementations.
//!
//! Each [`crate::model::NodeKind`] maps to a `NodeExecutor`: native control-flow
//! kinds resolve to executors in [`control_flow`], while capability-backed kinds
//! (which reach the outside world via [`crate::caps`]) resolve to executors in
//! [`integration`]. The engine dispatches each node to its executor through
//! `executor_for`.

pub mod control_flow;
pub mod integration;

use async_trait::async_trait;
use serde_json::Value;

use crate::caps::Capabilities;
use crate::data::Item;
use crate::error::Result;
use crate::model::{Node, NodeKind};

/// The runtime context handed to a node when it executes.
///
/// A node receives its resolved **input items** (the data-flow currency; see
/// [`crate::data`]) plus the run metadata, and returns a [`NodeOutput`].
pub struct NodeContext<'a> {
    /// The node being executed.
    pub node: &'a Node,
    /// The input items delivered to this node, resolved by the engine from the
    /// node's incoming edges. Nodes typically map their logic over these.
    pub input: &'a [Item],
    /// Run metadata and the trigger payload (the `run` slice of the run state).
    pub run: &'a Value,
    /// Host-provided capabilities.
    pub caps: &'a Capabilities,
}

/// The outcome of executing a single node: the items it emits and (for branching
/// nodes) which output port to follow.
#[derive(Debug, Clone, Default)]
pub struct NodeOutput {
    /// The items this node emits. A node maps over its input and returns an array
    /// of output [`Item`]s (which may be empty).
    pub items: Vec<Item>,
    /// For branching nodes, the output port to follow (e.g. `"true"`); `None`
    /// means the default `"main"` port.
    pub port: Option<String>,
}

impl NodeOutput {
    /// Builds an output on the default `"main"` port.
    #[must_use]
    pub fn main(items: Vec<Item>) -> Self {
        Self { items, port: None }
    }

    /// Builds an output that routes to the named `port`.
    #[must_use]
    pub fn routed(items: Vec<Item>, port: impl Into<String>) -> Self {
        Self {
            items,
            port: Some(port.into()),
        }
    }

    /// Builds an empty output on the default port (a node that produced no items).
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }
}

/// Executes one node kind.
#[async_trait]
pub trait NodeExecutor: Send + Sync {
    /// Runs the node and returns its output (or a routing decision).
    async fn execute(&self, ctx: NodeContext<'_>) -> Result<NodeOutput>;
}

/// A trigger node's executor: it echoes its input items through unchanged. The
/// engine seeds the trigger payload directly into the run state, so at runtime
/// the trigger is a passthrough; this executor makes the dispatch table total.
#[derive(Debug, Default, Clone)]
struct TriggerNode;

#[async_trait]
impl NodeExecutor for TriggerNode {
    async fn execute(&self, ctx: NodeContext<'_>) -> Result<NodeOutput> {
        Ok(NodeOutput::main(ctx.input.to_vec()))
    }
}

/// Returns the [`NodeExecutor`] for a given [`NodeKind`].
///
/// Native control-flow executors live in [`control_flow`]; capability-backed
/// ones in [`integration`]. The engine uses this to dispatch each graph node.
#[must_use]
pub(crate) fn executor_for(kind: &NodeKind) -> Box<dyn NodeExecutor> {
    match kind {
        NodeKind::Trigger => Box::new(TriggerNode),
        NodeKind::Agent => Box::new(integration::AgentNode),
        NodeKind::ToolCall => Box::new(integration::ToolCallNode),
        NodeKind::HttpRequest => Box::new(integration::HttpRequestNode),
        NodeKind::Code => Box::new(integration::CodeNode),
        NodeKind::OutputParser => Box::new(integration::OutputParserNode),
        NodeKind::SubWorkflow => Box::new(integration::SubWorkflowNode),
        NodeKind::Condition => Box::new(control_flow::ConditionNode),
        NodeKind::Switch => Box::new(control_flow::SwitchNode),
        NodeKind::Merge => Box::new(control_flow::MergeNode),
        NodeKind::SplitOut => Box::new(control_flow::SplitOutNode),
        NodeKind::Transform => Box::new(control_flow::TransformNode),
    }
}
