//! The `tool_call` node: one specific integration action.

use async_trait::async_trait;
use serde_json::Value;

use crate::data::Item;
use crate::error::{EngineError, Result};
use crate::nodes::integration::envelope;
use crate::nodes::{ExecutionMode, NodeContext, NodeExecutor, NodeOutput, execution_mode};

/// Invokes an integration action via [`crate::caps::ToolInvoker`].
///
/// **Execution** (`config.execution`, default `per_item`): in `per_item` mode
/// the node maps over its input, invoking the tool once per item with config
/// re-resolved against that item — so a `split_out` → `tool_call` fan-out runs
/// per element instead of silently dropping all but the first. `once` invokes a
/// single time against the first item. With no input, either mode invokes once.
///
/// Output is wrapped in the stable `{ json, text, raw }`
/// [envelope](crate::nodes::integration::envelope), matching the `agent` node so
/// a downstream `=item.json.<field>` binding is consistent across capabilities.
#[derive(Debug, Default, Clone)]
pub struct ToolCallNode;

/// Resolves `slug`/`args`/`connection_ref` from an already-resolved `cfg` and
/// invokes the tool, returning the raw provider result.
async fn invoke(ctx: &NodeContext<'_>, cfg: &Value) -> Result<Value> {
    let slug = cfg.get("slug").and_then(Value::as_str).ok_or_else(|| {
        EngineError::Capability("tool_call node: missing `slug` in config".to_string())
    })?;
    let args = cfg.get("args").cloned().unwrap_or(Value::Null);
    let conn = cfg.get("connection_ref").and_then(Value::as_str);
    ctx.caps.tools.invoke(slug, args, conn).await
}

#[async_trait]
impl NodeExecutor for ToolCallNode {
    async fn execute(&self, ctx: NodeContext<'_>) -> Result<NodeOutput> {
        let per_item = execution_mode(&ctx.node.config, ExecutionMode::PerItem)
            == ExecutionMode::PerItem
            && !ctx.input.is_empty();

        if per_item {
            // Map over the input: re-resolve config against each item (so
            // `=item.x` binds to the current item) and invoke once per item.
            // `config.concurrency` decides how many of those invocations are in
            // flight at once (default 1 — sequential, as before).
            let opts = crate::nodes::map::map_options(&ctx.node.config, &ctx.node.id, ctx.run);
            let ctx = &ctx;
            let (items, diagnostics) = crate::nodes::map::map_items(
                ctx.input.len(),
                &ctx.node.id,
                ctx.observer,
                opts,
                move |index| async move {
                    let (cfg, diags) = crate::nodes::resolve_config_traced_for_item(
                        ctx,
                        ctx.input[index].json.clone(),
                    );
                    let result = invoke(ctx, &cfg).await?;
                    Ok((Item::new(envelope::wrap(result)), diags))
                },
            )
            .await?;
            Ok(NodeOutput::main(items).with_diagnostics(diagnostics))
        } else {
            // Single invocation against the first-item scope (or empty input).
            let (cfg, diagnostics) = crate::nodes::resolve_config_traced(&ctx);
            let result = invoke(&ctx, &cfg).await?;
            Ok(NodeOutput::main(vec![Item::new(envelope::wrap(result))])
                .with_diagnostics(diagnostics))
        }
    }
}

#[cfg(test)]
#[path = "tool_call_tests.rs"]
mod tests;
