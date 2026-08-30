//! The `condition` node: a two-way IF branch.

use async_trait::async_trait;
use serde_json::Value;

use crate::error::Result;
use crate::expr;
use crate::nodes::{NodeContext, NodeExecutor, NodeOutput, expr_scope};

/// Two-way conditional branch, emitting on the `true` or `false` port.
#[derive(Debug, Default, Clone)]
pub struct ConditionNode;

#[async_trait]
impl NodeExecutor for ConditionNode {
    async fn execute(&self, ctx: NodeContext<'_>) -> Result<NodeOutput> {
        let field = ctx.node.config.get("field").and_then(Value::as_str);
        // `field` supports two shapes:
        // - a plain string (e.g. `"assignee"`) — a literal key looked up on the
        //   item's JSON, as before (backward-compatible);
        // - an `=`-expression (e.g. `"=item.assignee"`) — resolved against the
        //   node's expression scope (`item`/`items`/`run`/`nodes`) first, and
        //   the *resolved value itself* is what gets truthiness-checked. Without
        //   this, an expression field was read as a raw literal key
        //   (`item.json.get("=item.assignee")`), which never matches any real
        //   key and always routes `false`.
        let resolved_expr_value = field.filter(|f| expr::is_expression(f)).map(|f| {
            let scope = expr_scope(&ctx);
            expr::evaluate(&Value::String(f.to_string()), &scope)
        });
        let truthy = ctx.input.first().is_some_and(|item| {
            if let Some(value) = &resolved_expr_value {
                return is_truthy(value);
            }
            let value = match field {
                Some(f) => item.json.get(f).unwrap_or(&Value::Null),
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

#[cfg(test)]
#[path = "condition_tests.rs"]
mod tests;
