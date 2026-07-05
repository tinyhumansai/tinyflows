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
    /// The `nodes` slice of the run state: every node that has completed so far,
    /// keyed by node id, each slot shaped `{ "items": [<serialized Item>…] }`.
    /// This is what lets an expression address **any** upstream node's output by
    /// id (not just the direct predecessors delivered in `input`). Pass
    /// [`Value::Null`] when no run state exists (e.g. direct executor tests).
    pub nodes: &'a Value,
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
/// - `run` — the run metadata / trigger payload (`ctx.run`);
/// - `nodes` — every **completed** node's output, keyed by node id, each entry
///   shaped `{ "item": <first json>, "items": [<json>…] }`. This lets an
///   expression reference any upstream node by id — e.g.
///   `=nodes.fetch_recipient.item.email` or jq
///   `=.nodes["fetch_recipient"].items[0].email` — including non-adjacent
///   (grandparent) nodes and specific predecessors of a fan-in node. Node
///   **id** is the addressing key (stable across renames); names are not
///   indexed.
#[must_use]
pub(crate) fn expr_scope(ctx: &NodeContext) -> Value {
    let item = ctx
        .input
        .first()
        .map(|i| i.json.clone())
        .unwrap_or(Value::Null);
    let items: Vec<Value> = ctx.input.iter().map(|i| i.json.clone()).collect();
    serde_json::json!({
        "item": item,
        "items": items,
        "run": ctx.run,
        "nodes": nodes_scope(ctx.nodes),
    })
}

/// Projects the run state's `nodes` map into the expression-scope shape:
/// `{ "<id>": { "item": <first item json>, "items": [<item json>…] } }`.
///
/// Each state slot stores serialized [`Item`]s (`{ "json": …, … }`); the scope
/// exposes just the `json` payloads, mirroring how `item`/`items` are projected
/// from the node's own input. Slots without an `items` array (or a non-object
/// `nodes` value) are skipped, so an absent run state yields `{}`.
fn nodes_scope(nodes: &Value) -> Value {
    let mut scope = serde_json::Map::new();
    if let Value::Object(map) = nodes {
        for (id, slot) in map {
            let Some(items) = slot.get("items").and_then(Value::as_array) else {
                continue;
            };
            let jsons: Vec<Value> = items
                .iter()
                .map(|item| item.get("json").cloned().unwrap_or(Value::Null))
                .collect();
            let first = jsons.first().cloned().unwrap_or(Value::Null);
            scope.insert(
                id.clone(),
                serde_json::json!({ "item": first, "items": jsons }),
            );
        }
    }
    Value::Object(scope)
}

/// Resolves a node's config against its expression scope, tracing and logging
/// every `=`-expression that resolved to `null`.
///
/// The shared data-binding entry point for capability-backed nodes: the
/// resolved config is identical to `expr::resolve`'s, and each null-resolved
/// expression is `tracing::warn!`ed with the node id, config location, and the
/// original expression, then returned so the node can attach it to its
/// [`NodeOutput::diagnostics`]. Diagnostics are non-fatal by design — a null
/// may be intended, and failure policy belongs to routing/`on_error`.
pub(crate) fn resolve_config_traced(
    ctx: &NodeContext,
) -> (Value, Vec<crate::expr::NullResolution>) {
    let scope = expr_scope(ctx);
    let (cfg, misses) = crate::expr::resolve_traced(&ctx.node.config, &scope);
    for miss in &misses {
        tracing::warn!(
            node = %ctx.node.id,
            location = %miss.location,
            expression = %miss.expression,
            "config expression resolved to null; check the wiring (`nodes.<id>.item.<field>`)"
        );
    }
    (cfg, misses)
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
    /// Non-fatal data-binding diagnostics: every config `=`-expression that
    /// resolved to `null` during this execution (see
    /// [`crate::expr::resolve_traced`]). Surfaced on the run's
    /// [`ExecutionStep`](crate::observability::ExecutionStep) so a host can
    /// point at the exact unresolved wiring; failure policy stays with
    /// routing/`on_error`.
    pub diagnostics: Vec<crate::expr::NullResolution>,
}

impl NodeOutput {
    /// Builds an output on the default `"main"` port.
    #[must_use]
    pub fn main(items: Vec<Item>) -> Self {
        Self {
            items,
            ..Self::default()
        }
    }

    /// Builds an output that routes to the named `port`.
    #[must_use]
    pub fn routed(items: Vec<Item>, port: impl Into<String>) -> Self {
        Self {
            items,
            port: Some(port.into()),
            ..Self::default()
        }
    }

    /// Builds an empty output on the default port (a node that produced no items).
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Attaches data-binding diagnostics (null-resolved expressions) to this
    /// output.
    #[must_use]
    pub fn with_diagnostics(mut self, diagnostics: Vec<crate::expr::NullResolution>) -> Self {
        self.diagnostics = diagnostics;
        self
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
                    nodes: &Value::Null,
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
                nodes: &Value::Null,
                caps: &caps,
            })
            .await
            .expect("execute");
        assert_eq!(out.items, input);
        assert_eq!(out.port, None);
    }

    #[test]
    fn expr_scope_exposes_completed_nodes_keyed_by_id() {
        let caps = mock_capabilities();
        let run = Value::Null;
        let n = node(NodeKind::Transform, Value::Null);
        let input = vec![Item::new(json!({ "in": 1 }))];
        // Run-state shape: serialized `Item`s under each completed node's slot.
        let nodes_state = json!({
            "a": { "items": [
                { "json": { "x": 42 } },
                { "json": { "x": 43 }, "paired_item": 0 },
            ] },
            "b": { "items": [], "port": "true" },
            "broken": { "no_items": true },
        });
        let ctx = NodeContext {
            node: &n,
            input: &input,
            run: &run,
            nodes: &nodes_state,
            caps: &caps,
        };
        let scope = expr_scope(&ctx);
        // Existing keys unchanged (back-compat).
        assert_eq!(scope["item"], json!({ "in": 1 }));
        assert_eq!(scope["items"], json!([{ "in": 1 }]));
        // `nodes.<id>` projects each slot's item `json` payloads.
        assert_eq!(scope["nodes"]["a"]["item"], json!({ "x": 42 }));
        assert_eq!(
            scope["nodes"]["a"]["items"],
            json!([{ "x": 42 }, { "x": 43 }])
        );
        // An empty slot yields a null `item` and empty `items`.
        assert_eq!(scope["nodes"]["b"]["item"], Value::Null);
        assert_eq!(scope["nodes"]["b"]["items"], json!([]));
        // A slot without an `items` array is skipped, not panicked on.
        assert!(scope["nodes"].get("broken").is_none());
    }

    #[test]
    fn expr_scope_with_null_nodes_state_is_empty_map() {
        let caps = mock_capabilities();
        let run = Value::Null;
        let n = node(NodeKind::Transform, Value::Null);
        let ctx = NodeContext {
            node: &n,
            input: &[],
            run: &run,
            nodes: &Value::Null,
            caps: &caps,
        };
        let scope = expr_scope(&ctx);
        assert_eq!(scope["nodes"], json!({}));
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
