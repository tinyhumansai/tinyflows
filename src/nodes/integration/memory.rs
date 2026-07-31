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
            let opts = crate::nodes::map::map_options(&ctx.node.config, &ctx.node.id);
            let ctx = &ctx;
            let (items, diagnostics) =
                crate::nodes::map::map_items(ctx.input.len(), opts, move |index| async move {
                    let (cfg, diags) = crate::nodes::resolve_config_traced_for_item(
                        ctx,
                        ctx.input[index].json.clone(),
                    );
                    let result = call_provider(ctx, &cfg).await?;
                    Ok((Item::new(envelope::wrap(result)), diags))
                })
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
mod tests {
    use crate::caps::mock::{mock_capabilities, mock_capabilities_with_memory};
    use crate::compiler::compile;
    use crate::engine::run;
    use crate::model::{Edge, Node, NodeKind, WorkflowGraph};
    use serde_json::{Value, json};

    fn wf(config: Value) -> WorkflowGraph {
        WorkflowGraph {
            nodes: vec![
                Node {
                    id: "t".into(),
                    kind: NodeKind::Trigger,
                    type_version: 1,
                    name: "t".into(),
                    config: Value::Null,
                    ports: vec![],
                    position: None,
                },
                Node {
                    id: "n".into(),
                    kind: NodeKind::Memory,
                    type_version: 1,
                    name: "n".into(),
                    config,
                    ports: vec![],
                    position: None,
                },
            ],
            edges: vec![Edge {
                from_node: "t".into(),
                from_port: "main".into(),
                to_node: "n".into(),
                to_port: "main".into(),
            }],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn recall_executes_against_mock_and_emits_results_on_output() {
        let graph = wf(json!({ "operation": "recall", "scope": "flow", "query": "budget" }));
        let compiled = compile(&graph).expect("compile");
        let out = run(&compiled, Value::Null, &mock_capabilities())
            .await
            .expect("run");
        let results = &out.output["nodes"]["n"]["items"][0]["json"]["json"]["results"];
        assert!(
            results.as_array().is_some_and(|r| !r.is_empty()),
            "expected shaped recall results, got {results:?}"
        );
    }

    #[tokio::test]
    async fn flavour_operation_shapes_output() {
        let graph = wf(json!({ "operation": "flavour", "flavour": "email-tone" }));
        let compiled = compile(&graph).expect("compile");
        let out = run(&compiled, Value::Null, &mock_capabilities())
            .await
            .expect("run");
        assert_eq!(
            out.output["nodes"]["n"]["items"][0]["json"]["json"]["slug"],
            "email-tone"
        );
        assert!(out.output["nodes"]["n"]["items"][0]["json"]["json"]["traits"].is_object());
    }

    #[tokio::test]
    async fn people_operation_shapes_output() {
        let graph = wf(json!({ "operation": "people", "query": "cyrus" }));
        let compiled = compile(&graph).expect("compile");
        let out = run(&compiled, Value::Null, &mock_capabilities())
            .await
            .expect("run");
        let people = &out.output["nodes"]["n"]["items"][0]["json"]["json"]["people"];
        assert!(people.as_array().is_some_and(|p| !p.is_empty()));
    }

    #[tokio::test]
    async fn remember_flow_scope_writes_via_provider_and_passes_items_through() {
        let graph = wf(json!({
            "operation": "remember", "scope": "flow", "key": "k1", "value": { "v": 1 }
        }));
        let compiled = compile(&graph).expect("compile");
        let out = run(&compiled, Value::Null, &mock_capabilities())
            .await
            .expect("run");
        assert_eq!(
            out.output["nodes"]["n"]["items"][0]["json"]["json"]["ok"],
            true
        );
        assert_eq!(
            out.output["nodes"]["n"]["items"][0]["json"]["json"]["key"],
            "k1"
        );
    }

    #[tokio::test]
    async fn forget_flow_scope_executes() {
        let graph = wf(json!({ "operation": "forget", "scope": "flow", "key": "k1" }));
        let compiled = compile(&graph).expect("compile");
        let out = run(&compiled, Value::Null, &mock_capabilities())
            .await
            .expect("run");
        assert_eq!(
            out.output["nodes"]["n"]["items"][0]["json"]["json"]["operation"],
            "forget"
        );
    }

    use super::MemoryNode;
    use crate::data::Item;
    use crate::error::EngineError;
    use crate::nodes::{NodeContext, NodeExecutor};

    fn memory_node(config: Value) -> Node {
        Node {
            id: "n".into(),
            kind: NodeKind::Memory,
            type_version: 1,
            name: "n".into(),
            config,
            ports: vec![],
            position: None,
        }
    }

    #[tokio::test]
    async fn resolves_query_expression_per_item() {
        // `query="=item.id"` must resolve per-item before the provider call —
        // the `split_out` → `memory[recall]` dedupe pattern depends on this.
        let node = memory_node(json!({
            "operation": "recall", "scope": "flow", "query": "=item.id"
        }));
        let input = vec![
            Item::new(json!({ "id": "a" })),
            Item::new(json!({ "id": "b" })),
        ];
        let caps = mock_capabilities();
        let run_meta = Value::Null;
        let ctx = NodeContext {
            node: &node,
            input: &input,
            run: &run_meta,
            nodes: &Value::Null,
            caps: &caps,
        };
        let out = MemoryNode.execute(ctx).await.expect("execute");
        assert_eq!(out.items.len(), 2, "per_item default maps over input");
        assert_eq!(out.items[0].json["json"]["query"], "a");
        assert_eq!(out.items[1].json["json"]["query"], "b");
        assert_eq!(out.items[1].paired_item, Some(1));
    }

    #[tokio::test]
    async fn search_operation_passes_operation_through_opts() {
        // `search` reuses `recall` but tags `opts.operation` so a host that
        // distinguishes semantic recall from full-text search can branch on it.
        struct OptsEchoingMemory;
        #[async_trait::async_trait]
        impl crate::caps::MemoryProvider for OptsEchoingMemory {
            async fn recall(
                &self,
                _scope: &str,
                _query: &str,
                opts: Value,
            ) -> crate::error::Result<Value> {
                Ok(json!({ "opts": opts }))
            }
            async fn flavour(&self, _slug: &str) -> crate::error::Result<Value> {
                Ok(Value::Null)
            }
            async fn people(&self, _query: Option<&str>) -> crate::error::Result<Value> {
                Ok(Value::Null)
            }
            async fn remember(
                &self,
                _scope: &str,
                _key: &str,
                _value: Value,
            ) -> crate::error::Result<()> {
                Ok(())
            }
            async fn forget(&self, _scope: &str, _key: &str) -> crate::error::Result<()> {
                Ok(())
            }
        }

        let node = memory_node(json!({
            "operation": "search", "scope": "flow", "query": "x", "limit": 5
        }));
        let input: Vec<Item> = vec![];
        let caps = mock_capabilities_with_memory(OptsEchoingMemory);
        let run_meta = Value::Null;
        let ctx = NodeContext {
            node: &node,
            input: &input,
            run: &run_meta,
            nodes: &Value::Null,
            caps: &caps,
        };
        let out = MemoryNode.execute(ctx).await.expect("execute");
        assert_eq!(out.items[0].json["json"]["opts"]["operation"], "search");
        assert_eq!(out.items[0].json["json"]["opts"]["limit"], 5);
    }

    #[tokio::test]
    async fn missing_query_on_recall_is_a_capability_error() {
        let node = memory_node(json!({ "operation": "recall", "scope": "flow" }));
        let input = vec![];
        let caps = mock_capabilities();
        let run_meta = Value::Null;
        let ctx = NodeContext {
            node: &node,
            input: &input,
            run: &run_meta,
            nodes: &Value::Null,
            caps: &caps,
        };
        let err = MemoryNode
            .execute(ctx)
            .await
            .expect_err("missing query must error");
        assert!(
            matches!(err, EngineError::Capability(ref m) if m.contains("query")),
            "expected a capability error mentioning `query`, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn missing_key_on_remember_is_a_capability_error() {
        let node = memory_node(json!({
            "operation": "remember", "scope": "flow", "value": 1
        }));
        let input = vec![];
        let caps = mock_capabilities();
        let run_meta = Value::Null;
        let ctx = NodeContext {
            node: &node,
            input: &input,
            run: &run_meta,
            nodes: &Value::Null,
            caps: &caps,
        };
        let err = MemoryNode
            .execute(ctx)
            .await
            .expect_err("missing key must error");
        assert!(
            matches!(err, EngineError::Capability(ref m) if m.contains("key")),
            "expected a capability error mentioning `key`, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn write_to_non_flow_scope_is_a_capability_error_even_when_driven_directly() {
        // The validator rejects non-"flow" writes, but the executor is the last
        // line of defense when driven directly (bypassing validate). A
        // remember/forget against "flows" (read-only) or an absent scope must
        // hard-error, never silently write to the wrong place.
        for (op, cfg) in [
            (
                "remember",
                json!({ "operation": "remember", "scope": "flows", "key": "k", "value": 1 }),
            ),
            (
                "forget",
                json!({ "operation": "forget", "scope": "flows", "key": "k" }),
            ),
            // absent scope defaults to "" — also not "flow" — and must error.
            (
                "remember",
                json!({ "operation": "remember", "key": "k", "value": 1 }),
            ),
        ] {
            let node = memory_node(cfg);
            let input = vec![];
            let caps = mock_capabilities();
            let run_meta = Value::Null;
            let ctx = NodeContext {
                node: &node,
                input: &input,
                run: &run_meta,
                nodes: &Value::Null,
                caps: &caps,
            };
            let err = MemoryNode
                .execute(ctx)
                .await
                .expect_err("non-flow write must error");
            assert!(
                matches!(err, EngineError::Capability(ref m) if m.contains("flow") && m.contains(op)),
                "expected a capability error mentioning `flow` and `{op}`, got: {err:?}"
            );
        }
    }

    #[tokio::test]
    async fn missing_operation_is_a_capability_error() {
        let node = memory_node(json!({ "scope": "flow" }));
        let input = vec![];
        let caps = mock_capabilities();
        let run_meta = Value::Null;
        let ctx = NodeContext {
            node: &node,
            input: &input,
            run: &run_meta,
            nodes: &Value::Null,
            caps: &caps,
        };
        let err = MemoryNode
            .execute(ctx)
            .await
            .expect_err("missing operation must error");
        assert!(
            matches!(err, EngineError::Capability(ref m) if m.contains("operation")),
            "expected a capability error mentioning `operation`, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn no_memory_provider_wired_is_a_capability_error() {
        let node = memory_node(json!({ "operation": "people" }));
        let input = vec![];
        let mut caps = mock_capabilities();
        caps.memory = None;
        let run_meta = Value::Null;
        let ctx = NodeContext {
            node: &node,
            input: &input,
            run: &run_meta,
            nodes: &Value::Null,
            caps: &caps,
        };
        let err = MemoryNode
            .execute(ctx)
            .await
            .expect_err("no MemoryProvider must error");
        assert!(
            matches!(err, EngineError::Capability(ref m) if m.contains("MemoryProvider")),
            "expected a capability error mentioning MemoryProvider, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn execution_once_collapses_the_batch_to_a_single_call() {
        let node = memory_node(json!({
            "operation": "recall", "scope": "flow", "query": "=item.id", "execution": "once"
        }));
        let input = vec![
            Item::new(json!({ "id": "a" })),
            Item::new(json!({ "id": "b" })),
        ];
        let caps = mock_capabilities();
        let run_meta = Value::Null;
        let ctx = NodeContext {
            node: &node,
            input: &input,
            run: &run_meta,
            nodes: &Value::Null,
            caps: &caps,
        };
        let out = MemoryNode.execute(ctx).await.expect("execute");
        assert_eq!(out.items.len(), 1, "once mode emits a single item");
        assert_eq!(out.items[0].json["json"]["query"], "a");
    }
}
