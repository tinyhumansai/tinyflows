//! The `output_parser` node: structures/validates an agent's output.

use async_trait::async_trait;
use serde_json::Value;

use crate::data::Item;
use crate::error::Result;
use crate::nodes::integration::schema;
use crate::nodes::{NodeContext, NodeExecutor, NodeOutput};

/// Parses / validates an upstream agent's output into a structured shape.
///
/// When the node's `config.schema` holds a (subset-of-)JSON-Schema, each input
/// item's `json` is validated against it; on failure the node makes one LLM
/// auto-fix attempt (via the injected [`crate::caps::LlmProvider`]) and
/// re-validates, emitting the repaired value. A value that still fails — or fails
/// with `config.auto_fix == false` — surfaces a capability error, which the
/// engine routes per the node's `on_error` policy (`stop` / `continue` /
/// `route`). See [`schema`] for the supported schema subset.
///
/// Config:
/// - `schema` — the JSON Schema to validate against. Omitted / null ⇒ the node is
///   an identity passthrough (back-compat with the pre-validation behavior).
/// - `auto_fix` — whether to attempt the one-shot LLM repair (default `true`).
/// - `connection_ref` — optional opaque credential id for the auto-fix LLM call.
#[derive(Debug, Default, Clone)]
pub struct OutputParserNode;

#[async_trait]
impl NodeExecutor for OutputParserNode {
    async fn execute(&self, ctx: NodeContext<'_>) -> Result<NodeOutput> {
        let cfg = &ctx.node.config;
        // Back-compat: with no schema configured the node is an identity
        // passthrough of its input items.
        let schema_val = match cfg.get("schema") {
            Some(s) if !s.is_null() => s,
            _ => return Ok(NodeOutput::main(ctx.input.to_vec())),
        };
        let auto_fix = cfg.get("auto_fix").and_then(Value::as_bool).unwrap_or(true);
        let conn = cfg.get("connection_ref").and_then(Value::as_str);

        let mut out = Vec::with_capacity(ctx.input.len());
        for item in ctx.input {
            let validated = schema::parse_and_validate(
                item.json.clone(),
                schema_val,
                auto_fix,
                &ctx.caps.llm,
                conn,
            )
            .await?;
            out.push(Item::new(validated));
        }
        Ok(NodeOutput::main(out))
    }
}

#[cfg(test)]
#[path = "output_parser_tests.rs"]
mod tests;
