//! The `condition` node: a two-way IF branch.

use async_trait::async_trait;

use crate::error::Result;
use crate::nodes::{NodeContext, NodeExecutor, NodeOutput};

/// Two-way conditional branch, emitting on the `true` or `false` port.
#[derive(Debug, Default, Clone)]
pub struct ConditionNode;

#[async_trait]
impl NodeExecutor for ConditionNode {
    async fn execute(&self, ctx: NodeContext<'_>) -> Result<NodeOutput> {
        let field = ctx
            .node
            .config
            .get("field")
            .and_then(serde_json::Value::as_str);
        let truthy = ctx.input.first().is_some_and(|item| {
            let value = match field {
                Some(f) => item.json.get(f).unwrap_or(&serde_json::Value::Null),
                None => &item.json,
            };
            is_truthy(value)
        });
        Ok(NodeOutput::routed(
            ctx.input.to_vec(),
            if truthy { "true" } else { "false" },
        ))
    }
}

/// Truthiness predicate: `null`, `false`, `0`, `""`, `[]`, and `{}` are falsey;
/// every other value is truthy.
fn is_truthy(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Null => false,
        serde_json::Value::Bool(b) => *b,
        serde_json::Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        serde_json::Value::String(s) => !s.is_empty(),
        serde_json::Value::Array(a) => !a.is_empty(),
        serde_json::Value::Object(o) => !o.is_empty(),
    }
}
