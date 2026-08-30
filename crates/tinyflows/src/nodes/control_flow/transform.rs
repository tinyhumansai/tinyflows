//! The `transform` node: a pure, expression-based data transform.

use async_trait::async_trait;

use crate::data::Item;
use crate::error::Result;
use crate::nodes::{NodeContext, NodeExecutor, NodeOutput};

/// Pure, expression-based data transform over the run state.
#[derive(Debug, Default, Clone)]
pub struct TransformNode;

#[async_trait]
impl NodeExecutor for TransformNode {
    async fn execute(&self, ctx: NodeContext<'_>) -> Result<NodeOutput> {
        let set = ctx
            .node
            .config
            .get("set")
            .and_then(serde_json::Value::as_object)
            .cloned();
        let items: Vec<serde_json::Value> = ctx.input.iter().map(|i| i.json.clone()).collect();
        // Project the run-state `nodes` map once; each per-item scope reuses it so
        // a `set` expression can address upstream nodes by id
        // (`=nodes.<id>.item.<field>`), consistent with the integration nodes.
        let nodes = crate::nodes::nodes_scope(ctx.nodes);
        let mut out = Vec::with_capacity(ctx.input.len());
        for (index, item) in ctx.input.iter().enumerate() {
            // `item` is this loop's current item; `items` exposes the full input
            // batch; `nodes` addresses any completed upstream node by id.
            // Built through the shared constructor so this node can never drift
            // out of sync with the scope every other node sees.
            let scope = crate::nodes::build_expr_scope(
                item.json.clone(),
                items.clone(),
                ctx.run,
                nodes.clone(),
            );
            let mut json = item.json.clone();
            if let Some(set) = &set {
                if !json.is_object() {
                    json = serde_json::Value::Object(serde_json::Map::new());
                }
                if let Some(obj) = json.as_object_mut() {
                    for (key, expr) in set {
                        obj.insert(key.clone(), crate::expr::evaluate(expr, &scope));
                    }
                }
            }
            out.push(Item::new(json).paired_with(index));
        }
        Ok(NodeOutput::main(out))
    }
}

#[cfg(test)]
#[path = "transform_tests.rs"]
mod tests;
