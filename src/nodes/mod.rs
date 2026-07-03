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

/// Builds the expression scope for a node from its runtime [`NodeContext`].
///
/// The returned object is the `.` input for `=`-expressions evaluated over a
/// node's config (see [`crate::expr::resolve`]). It exposes exactly what
/// `NodeContext` makes available:
///
/// - `item` — the first input item's `json`, or [`Value::Null`] when there is
///   no input;
/// - `items` — the `json` of every input item, in order;
/// - `run` — the run metadata / trigger payload (`ctx.run`).
#[must_use]
pub(crate) fn expr_scope(ctx: &NodeContext) -> Value {
    let item = ctx
        .input
        .first()
        .map(|i| i.json.clone())
        .unwrap_or(Value::Null);
    let items: Vec<Value> = ctx.input.iter().map(|i| i.json.clone()).collect();
    serde_json::json!({ "item": item, "items": items, "run": ctx.run })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::mock::mock_capabilities;
    use crate::data::Item;
    use crate::model::Node;
    use serde_json::json;

    /// Every [`NodeKind`] variant, so the coverage below stays exhaustive.
    fn all_kinds() -> Vec<NodeKind> {
        use NodeKind::{
            Agent, Code, Condition, HttpRequest, Merge, OutputParser, SplitOut, SubWorkflow,
            Switch, ToolCall, Transform, Trigger,
        };
        vec![
            Trigger,
            Agent,
            ToolCall,
            HttpRequest,
            Code,
            Condition,
            Switch,
            Merge,
            SplitOut,
            Transform,
            OutputParser,
            SubWorkflow,
        ]
    }

    /// Minimal config that lets each kind execute successfully.
    fn config_for(kind: &NodeKind) -> Value {
        match kind {
            NodeKind::ToolCall => json!({ "slug": "demo" }),
            NodeKind::SubWorkflow => json!({
                "workflow": { "nodes": [{ "id": "ct", "kind": "trigger", "name": "ct" }], "edges": [] }
            }),
            _ => Value::Null,
        }
    }

    fn node(kind: NodeKind, config: Value) -> Node {
        Node {
            id: "n".into(),
            kind,
            type_version: 1,
            name: "n".into(),
            config,
            ports: vec![],
            position: None,
        }
    }

    #[tokio::test]
    async fn executor_for_is_total_and_every_executor_runs() {
        let caps = mock_capabilities();
        let run = Value::Null;
        for kind in all_kinds() {
            let node = node(kind.clone(), config_for(&kind));
            let input = vec![Item::new(json!({ "x": 1 }))];
            let exec = executor_for(&kind);
            let out = exec
                .execute(NodeContext {
                    node: &node,
                    input: &input,
                    run: &run,
                    caps: &caps,
                })
                .await;
            assert!(
                out.is_ok(),
                "executor for {kind:?} should run: {:?}",
                out.err()
            );
        }
    }

    #[tokio::test]
    async fn trigger_executor_passes_input_through() {
        let caps = mock_capabilities();
        let run = Value::Null;
        let node = node(NodeKind::Trigger, Value::Null);
        let input = vec![Item::new(json!({ "a": 1 })), Item::new(json!({ "b": 2 }))];
        let out = executor_for(&NodeKind::Trigger)
            .execute(NodeContext {
                node: &node,
                input: &input,
                run: &run,
                caps: &caps,
            })
            .await
            .expect("execute");
        assert_eq!(out.items, input);
        assert_eq!(out.port, None);
    }

    #[test]
    fn node_output_constructors_have_expected_shapes() {
        let items = vec![Item::new(json!({ "a": 1 }))];

        let main = NodeOutput::main(items.clone());
        assert_eq!(main.port, None);
        assert_eq!(main.items, items);

        let routed = NodeOutput::routed(items.clone(), "true");
        assert_eq!(routed.port.as_deref(), Some("true"));
        assert_eq!(routed.items, items);

        let empty = NodeOutput::empty();
        assert!(empty.items.is_empty());
        assert_eq!(empty.port, None);
    }
}
