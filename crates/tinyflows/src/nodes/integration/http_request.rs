//! The `http_request` node: an outbound HTTP request.

use async_trait::async_trait;
use serde_json::Value;

use crate::data::Item;
use crate::error::Result;
use crate::nodes::integration::envelope;
use crate::nodes::{ExecutionMode, NodeContext, NodeExecutor, NodeOutput, execution_mode};

/// Performs an outbound HTTP request via [`crate::caps::HttpClient`].
///
/// **Execution** (`config.execution`, default `per_item`): `per_item` maps over
/// the input, issuing one request per item with the descriptor re-resolved
/// against that item; `once` issues a single request against the first item.
/// Output is wrapped in the stable `{ json, text, raw }`
/// [envelope](crate::nodes::integration::envelope), matching the other
/// capability nodes.
#[derive(Debug, Default, Clone)]
pub struct HttpRequestNode;

/// Issues the request described by an already-resolved `cfg`.
async fn request(ctx: &NodeContext<'_>, cfg: &Value) -> Result<Value> {
    // The node's config is the request descriptor; the host's HttpClient interprets it.
    let conn = cfg.get("connection_ref").and_then(Value::as_str);
    ctx.caps.http.request(cfg.clone(), conn).await
}

#[async_trait]
impl NodeExecutor for HttpRequestNode {
    async fn execute(&self, ctx: NodeContext<'_>) -> Result<NodeOutput> {
        let per_item = execution_mode(&ctx.node.config, ExecutionMode::PerItem)
            == ExecutionMode::PerItem
            && !ctx.input.is_empty();

        if per_item {
            // `config.concurrency` decides how many requests are in flight at
            // once (default 1 — sequential, as before).
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
                    let response = request(ctx, &cfg).await?;
                    Ok((Item::new(envelope::wrap(response)), diags))
                },
            )
            .await?;
            Ok(NodeOutput::main(items).with_diagnostics(diagnostics))
        } else {
            let (cfg, diagnostics) = crate::nodes::resolve_config_traced(&ctx);
            let response = request(&ctx, &cfg).await?;
            Ok(NodeOutput::main(vec![Item::new(envelope::wrap(response))])
                .with_diagnostics(diagnostics))
        }
    }
}

#[cfg(test)]
#[path = "http_request_tests.rs"]
mod tests;
