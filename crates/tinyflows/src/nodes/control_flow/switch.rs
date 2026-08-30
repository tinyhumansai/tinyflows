//! The `switch` node: a multi-way branch.

use async_trait::async_trait;

use crate::error::Result;
use crate::nodes::{NodeContext, NodeExecutor, NodeOutput};

/// Multi-way branch keyed by a computed case value.
///
/// The case key comes from config: an `expression` (an `=`-expression evaluated
/// against the `{ item, items, run, nodes }` node scope) takes precedence,
/// otherwise a `field` names a key on the first input item. The resulting value
/// selects the output port to emit on, routing to the matching case; a `null`
/// result routes to the `default` port.
#[derive(Debug, Default, Clone)]
pub struct SwitchNode;

#[async_trait]
impl NodeExecutor for SwitchNode {
    async fn execute(&self, ctx: NodeContext<'_>) -> Result<NodeOutput> {
        // Use the shared node scope so `expression` can address upstream nodes by
        // id (`=nodes.<id>.item.<field>`) — same `{ item, items, run, nodes }`
        // scope the integration nodes get. `item` is the first input item.
        let scope = crate::nodes::expr_scope(&ctx);
        let value = if let Some(expr) = ctx.node.config.get("expression") {
            crate::expr::evaluate(expr, &scope)
        } else if let Some(field) = ctx
            .node
            .config
            .get("field")
            .and_then(serde_json::Value::as_str)
        {
            scope["item"]
                .get(field)
                .cloned()
                .unwrap_or(serde_json::Value::Null)
        } else {
            serde_json::Value::Null
        };
        // Map the discriminant to a port name. Only scalar values name a port
        // sensibly: a string is used verbatim, and a number/bool uses its natural
        // rendering (`42`, `true`) so switching on a numeric/boolean field works
        // predictably. A `null` or a non-scalar (object/array) has no meaningful
        // port name — dumping its JSON as a port would never match a real port and
        // is a confusing footgun — so those route to the `default` fallback port.
        let port = match value {
            serde_json::Value::String(s) => s,
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::Bool(b) => b.to_string(),
            serde_json::Value::Null
            | serde_json::Value::Object(_)
            | serde_json::Value::Array(_) => "default".to_string(),
        };
        Ok(NodeOutput::routed(ctx.input.to_vec(), port))
    }
}

#[cfg(test)]
#[path = "switch_tests.rs"]
mod tests;
