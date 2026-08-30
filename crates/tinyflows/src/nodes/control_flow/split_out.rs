//! The `split_out` node: per-item fan-out.

use async_trait::async_trait;

use crate::data::Item;
use crate::error::Result;
use crate::nodes::{NodeContext, NodeExecutor, NodeOutput};

/// Fan-out that emits one item per element of a list.
#[derive(Debug, Default, Clone)]
pub struct SplitOutNode;

/// Resolve a dotted `path` (e.g. `"json.data.messages"`) against a JSON value,
/// traversing one object key per `.`-separated segment. A single-segment path
/// (no dots) is a plain key lookup — so existing `path: "items"` configs keep
/// working unchanged. A missing intermediate/leaf key yields `Null`.
///
/// This is what lets `split_out` reach an array nested inside a `tool_call`'s
/// `{json,text,raw}` envelope: `path: "json.data.messages"` walks
/// envelope → tool result → the array (Composio actions like
/// `GMAIL_FETCH_EMAILS` return their list at `data.messages`, not a top-level
/// key).
fn resolve_dotted_path(value: &serde_json::Value, path: &str) -> serde_json::Value {
    let mut current = value;
    for segment in path.split('.') {
        match current.get(segment) {
            Some(v) => current = v,
            None => return serde_json::Value::Null,
        }
    }
    current.clone()
}

#[async_trait]
impl NodeExecutor for SplitOutNode {
    async fn execute(&self, ctx: NodeContext<'_>) -> Result<NodeOutput> {
        let path = ctx
            .node
            .config
            .get("path")
            .and_then(serde_json::Value::as_str);
        let mut out = Vec::new();
        for (index, item) in ctx.input.iter().enumerate() {
            let value = match path {
                Some(p) if !p.is_empty() => resolve_dotted_path(&item.json, p),
                _ => item.json.clone(),
            };
            match value {
                serde_json::Value::Array(elements) => {
                    for element in elements {
                        out.push(Item::new(element).paired_with(index));
                    }
                }
                other => out.push(Item::new(other).paired_with(index)),
            }
        }
        Ok(NodeOutput::main(out))
    }
}

#[cfg(test)]
#[path = "split_out_tests.rs"]
mod tests;
