//! The `memory` node: recall/search/flavour/people/remember/forget against the
//! host's injected [`crate::caps::MemoryProvider`].
//!
//! A **delivery surface**, not a capability of its own — it exposes whatever
//! durable memory store the host already maintains to the declarative graph, so
//! a node kind that "cannot reason" (a `condition` branch, a `transform`) can
//! still gate or bind on recalled memory without an agent turn in the loop. See
//! the crate's design notes for the full rationale.
//!
//! Config (`config.operation` selects the shape read below):
//!
//! | Field | Used by | Required |
//! |---|---|---|
//! | `operation` | all | always: `recall`\|`search`\|`flavour`\|`people`\|`remember`\|`forget` |
//! | `scope` | recall, remember, forget | yes (host-defined: `"user"`\|`"flow"`\|`"flows"`) |
//! | `query` | recall, search | yes (`=`-bindable) |
//! | `flavour` | flavour | yes (a slug string) |
//! | `key` | remember, forget | yes (`=`-bindable) |
//! | `value` | remember | yes (`=`-bindable) |
//! | `limit`, `min_score` | recall, search | no |
//!
//! [`crate::validate`] enforces the hard security invariant — `remember`/
//! `forget` may never carry `scope: "user"` — and the required-field checks
//! above, structurally, before a run starts. This executor still defends
//! against a missing/malformed config (e.g. when driven directly in a test,
//! bypassing the validator) with the same [`crate::error::EngineError::Capability`]
//! pattern every other integration node uses for a bad config.

use async_trait::async_trait;
use serde_json::Value;

use crate::data::Item;
use crate::error::{EngineError, Result};
use crate::nodes::integration::envelope;
use crate::nodes::{ExecutionMode, NodeContext, NodeExecutor, NodeOutput, execution_mode};

/// Stable `tracing` grep prefix for every log line this node emits.
const LOG_PREFIX: &str = "[memory-node]";

/// Reads/writes host-managed memory via
/// [`MemoryProvider`](crate::caps::MemoryProvider).
///
/// **Execution** (`config.execution`, default `per_item`): matches `tool_call` /
/// `http_request` — in `per_item` mode the node maps over its input, calling
/// the provider once per item with config re-resolved against that item (the
/// `split_out` → `memory[recall]` dedupe-check pattern depends on this: each
/// candidate's own `=item.title` must reach its own recall call). `once`
/// invokes a single time against the first item. With no input, either mode
/// invokes once.
///
/// Output is wrapped in the stable `{ json, text, raw }`
/// [envelope](crate::nodes::integration::envelope), matching every other
/// capability node.
#[derive(Debug, Default, Clone)]
pub struct MemoryNode;

/// Resolves `operation`/`scope`/`query`/etc. from an already-resolved `cfg` and
/// calls the matching [`MemoryProvider`](crate::caps::MemoryProvider) method,
/// returning the provider's (unenveloped) result.
async fn call_provider(ctx: &NodeContext<'_>, cfg: &Value) -> Result<Value> {
    let provider = ctx.caps.memory.as_ref().ok_or_else(|| {
        EngineError::Capability(
            "memory node: host has not wired a MemoryProvider capability".to_string(),
        )
    })?;

    let operation = cfg
        .get("operation")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            EngineError::Capability("memory node: missing `operation` in config".to_string())
        })?;
    let scope = cfg.get("scope").and_then(Value::as_str).unwrap_or("");

    tracing::debug!(
        node = %ctx.node.id,
        operation,
        scope,
        "{LOG_PREFIX} executing"
    );

    let result = match operation {
        "recall" | "search" => {
            let query = cfg.get("query").and_then(Value::as_str).ok_or_else(|| {
                EngineError::Capability(format!(
                    "memory node: `{operation}` operation requires `query`"
                ))
            })?;
            tracing::debug!(
                node = %ctx.node.id,
                query,
                "{LOG_PREFIX} resolved query, calling provider.recall"
            );
            let mut opts = serde_json::Map::new();
            opts.insert(
                "operation".to_string(),
                Value::String(operation.to_string()),
            );
            if let Some(limit) = cfg.get("limit") {
                opts.insert("limit".to_string(), limit.clone());
            }
            if let Some(min_score) = cfg.get("min_score") {
                opts.insert("min_score".to_string(), min_score.clone());
            }
            provider.recall(scope, query, Value::Object(opts)).await?
        }
        "flavour" => {
            let slug = cfg.get("flavour").and_then(Value::as_str).ok_or_else(|| {
                EngineError::Capability(
                    "memory node: `flavour` operation requires `flavour` (slug)".to_string(),
                )
            })?;
            tracing::debug!(
                node = %ctx.node.id,
                slug,
                "{LOG_PREFIX} calling provider.flavour"
            );
            provider.flavour(slug).await?
        }
        "people" => {
            let query = cfg.get("query").and_then(Value::as_str);
            tracing::debug!(
                node = %ctx.node.id,
                query = query.unwrap_or("<none>"),
                "{LOG_PREFIX} calling provider.people"
            );
            provider.people(query).await?
        }
        "remember" => {
            // Defense-in-depth (the validator already rejects non-"flow" writes,
            // but this executor may be driven directly): writes go ONLY to
            // scope "flow". An absent scope (defaulted to "") or a read-only
            // scope ("user"/"flows") is a hard error, never a silent write to
            // the wrong place.
            if scope != "flow" {
                return Err(EngineError::Capability(format!(
                    "memory node: `remember` may only write scope \"flow\", got {scope:?}"
                )));
            }
            let key = cfg.get("key").and_then(Value::as_str).ok_or_else(|| {
                EngineError::Capability(
                    "memory node: `remember` operation requires `key`".to_string(),
                )
            })?;
            let value = cfg.get("value").cloned().ok_or_else(|| {
                EngineError::Capability(
                    "memory node: `remember` operation requires `value`".to_string(),
                )
            })?;
            tracing::debug!(
                node = %ctx.node.id,
                key,
                "{LOG_PREFIX} calling provider.remember"
            );
            provider.remember(scope, key, value).await?;
            serde_json::json!({ "ok": true, "operation": "remember", "key": key })
        }
        "forget" => {
            // Same flow-only write invariant as `remember` (see above).
            if scope != "flow" {
                return Err(EngineError::Capability(format!(
                    "memory node: `forget` may only write scope \"flow\", got {scope:?}"
                )));
            }
            let key = cfg.get("key").and_then(Value::as_str).ok_or_else(|| {
                EngineError::Capability(
                    "memory node: `forget` operation requires `key`".to_string(),
                )
            })?;
            tracing::debug!(
                node = %ctx.node.id,
                key,
                "{LOG_PREFIX} calling provider.forget"
            );
            provider.forget(scope, key).await?;
            serde_json::json!({ "ok": true, "operation": "forget", "key": key })
        }
        other => {
            return Err(EngineError::Capability(format!(
                "memory node: unknown operation {other:?}"
            )));
        }
    };

    tracing::debug!(
        node = %ctx.node.id,
        operation,
        result_size = result_size(&result),
        "{LOG_PREFIX} provider call returned"
    );
    Ok(result)
}

/// A coarse "how big is this" hint for the debug log — the length of an array
/// result (e.g. `recall`'s `results`), the object's field count, or `1` for a
/// scalar/`null`. Not part of the node's output contract, purely diagnostic.
fn result_size(value: &Value) -> usize {
    match value {
        Value::Array(items) => items.len(),
        Value::Object(map) => {
            for field in map.values() {
                if let Some(arr) = field.as_array() {
                    return arr.len();
                }
            }
            map.len()
        }
        _ => 1,
    }
}

#[async_trait]
impl NodeExecutor for MemoryNode {
    async fn execute(&self, ctx: NodeContext<'_>) -> Result<NodeOutput> {
        let per_item = execution_mode(&ctx.node.config, ExecutionMode::PerItem)
            == ExecutionMode::PerItem
            && !ctx.input.is_empty();

        tracing::debug!(
            node = %ctx.node.id,
            per_item,
            input_len = ctx.input.len(),
            "{LOG_PREFIX} entering execute"
        );

        if per_item {
            // `config.concurrency` decides how many provider calls are in flight
            // at once (default 1 — sequential, as before).
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
                    let result = call_provider(ctx, &cfg).await?;
                    Ok((Item::new(envelope::wrap(result)), diags))
                },
            )
            .await?;
            tracing::debug!(
                node = %ctx.node.id,
                emitted = items.len(),
                "{LOG_PREFIX} exiting execute (per_item)"
            );
            Ok(NodeOutput::main(items).with_diagnostics(diagnostics))
        } else {
            let (cfg, diagnostics) = crate::nodes::resolve_config_traced(&ctx);
            let result = call_provider(&ctx, &cfg).await?;
            tracing::debug!(
                node = %ctx.node.id,
                "{LOG_PREFIX} exiting execute (once)"
            );
            Ok(NodeOutput::main(vec![Item::new(envelope::wrap(result))])
                .with_diagnostics(diagnostics))
        }
    }
}

#[cfg(test)]
#[path = "memory_tests.rs"]
mod tests;
