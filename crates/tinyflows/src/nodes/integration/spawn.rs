//! The `spawn` node: start work, don't wait for it.
//!
//! Every other node in the catalog blocks its branch until it has an answer.
//! This one starts the work through the [`TaskRunner`](crate::caps::TaskRunner)
//! capability and immediately emits a **ticket**, so the branch continues while
//! the work runs. A downstream [`gate`](super::gate) turns tickets back into
//! results.
//!
//! # Degrading without a `TaskRunner`
//!
//! A host that injects none still runs these graphs: the work is performed
//! inline and the ticket comes back already settled. The answer is identical;
//! what is lost is the overlap. That is worth stating loudly rather than
//! leaving as a surprise, because it is a performance cliff that no test of
//! correctness will catch.

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::caps::{TaskSpec, TaskState};
use crate::data::Item;
use crate::error::{EngineError, Result};
use crate::nodes::{NodeContext, NodeExecutor, NodeOutput};

/// The key a spawned task's ticket is emitted under. A `gate` reads this back
/// when it is pointed at a spawn node rather than at an expression.
pub(crate) const TICKET_KEY: &str = "ticket";

/// Starts background work and emits one ticket item per started task.
#[derive(Debug, Default, Clone)]
pub struct SpawnNode;

/// Builds the [`TaskSpec`] a node's config describes.
fn spec_from_config(config: &Value, node_id: &str) -> Result<TaskSpec> {
    match config.get("target").and_then(Value::as_str) {
        Some("workflow") => {
            let graph = config.get("workflow").cloned().ok_or_else(|| {
                EngineError::Capability(format!(
                    "spawn node {node_id:?}: `target: \"workflow\"` needs a `workflow` graph"
                ))
            })?;
            Ok(TaskSpec::Workflow {
                graph,
                input: config.get("input").cloned().unwrap_or(Value::Null),
            })
        }
        Some("tool") => {
            let slug = config
                .get("slug")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    EngineError::Capability(format!(
                        "spawn node {node_id:?}: `target: \"tool\"` needs a `slug`"
                    ))
                })?
                .to_string();
            Ok(TaskSpec::Tool {
                slug,
                args: config.get("args").cloned().unwrap_or(Value::Null),
            })
        }
        Some("http") => Ok(TaskSpec::Http {
            request: config.get("request").cloned().unwrap_or(Value::Null),
        }),
        Some(other) => Err(EngineError::Capability(format!(
            "spawn node {node_id:?}: unknown `target` {other:?}; expected workflow, tool or http"
        ))),
        None => Err(EngineError::Capability(format!(
            "spawn node {node_id:?}: missing `target` (workflow, tool or http)"
        ))),
    }
}

/// Runs `spec` inline, for a host that injected no [`TaskRunner`].
///
/// Returns the result directly; the caller wraps it in a settled ticket so
/// downstream sees the same shape either way.
async fn run_inline(spec: TaskSpec, ctx: &NodeContext<'_>) -> Result<Value> {
    match spec {
        TaskSpec::Tool { slug, args } => ctx.caps.tools.invoke(&slug, args, None).await,
        TaskSpec::Http { request } => ctx.caps.http.request(request, None).await,
        TaskSpec::Workflow { graph, input } => {
            let child: crate::model::WorkflowGraph =
                serde_json::from_value(graph).map_err(|e| {
                    EngineError::Capability(format!("spawn node: invalid workflow: {e}"))
                })?;
            let compiled = crate::compiler::compile(&child)?;
            let outcome = Box::pin(crate::engine::run(&compiled, input, ctx.caps)).await?;
            Ok(outcome.output)
        }
    }
}

#[async_trait]
impl NodeExecutor for SpawnNode {
    async fn execute(&self, ctx: NodeContext<'_>) -> Result<NodeOutput> {
        let spec = spec_from_config(&ctx.node.config, &ctx.node.id)?;

        let item = match ctx.caps.tasks.as_ref() {
            Some(runner) => {
                let ticket = runner.start(spec).await?;
                tracing::debug!(node = %ctx.node.id, %ticket, "spawn: started background task");
                Item::new(json!({
                    TICKET_KEY: ticket,
                    "spawn": ctx.node.id,
                    "started_at_step": ctx.step,
                }))
            }
            None => {
                // No runner: do the work here and hand back a ticket that is
                // already settled, so a downstream gate needs no special case.
                tracing::debug!(
                    node = %ctx.node.id,
                    "spawn: no TaskRunner injected; running inline (no overlap)"
                );
                let result = run_inline(spec, &ctx).await?;
                Item::new(json!({
                    TICKET_KEY: Value::Null,
                    "spawn": ctx.node.id,
                    "started_at_step": ctx.step,
                    "inline": true,
                    "result": result,
                }))
            }
        };
        Ok(NodeOutput::main(vec![item]))
    }
}

/// Reads an already-settled inline result off a ticket item, if it has one.
///
/// A gate uses this so an inline-degraded spawn collects without ever asking
/// the (absent) runner about a ticket that does not exist.
pub(crate) fn inline_result(item: &Value) -> Option<TaskState> {
    item.get("inline")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        .then(|| TaskState::Done(item.get("result").cloned().unwrap_or(Value::Null)))
}

#[cfg(test)]
#[path = "spawn_tests.rs"]
mod tests;
