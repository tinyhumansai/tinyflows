//! The `sub_workflow` node: runs another workflow as a nested sub-graph.

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::engine::MAX_SUB_WORKFLOW_DEPTH;
use crate::error::{EngineError, Result};
use crate::model::{NodeKind, WorkflowGraph};
use crate::nodes::{NodeContext, NodeExecutor, NodeOutput};

/// Runs another workflow as a nested sub-graph.
///
/// The child [`WorkflowGraph`](crate::model::WorkflowGraph) is supplied one of
/// two ways — **exactly one** of these config keys must be present:
///
/// - `workflow` — the child graph embedded **inline** as JSON (back-compat;
///   the original behavior).
/// - `workflow_id` — a host-managed **reference** to a saved workflow. The
///   engine is persistence-free, so it resolves the id to a graph through the
///   host-injected [`WorkflowResolver`](crate::caps::WorkflowResolver)
///   (`ctx.caps.resolver`).
///
/// The resolved child is compiled and run via [`crate::engine::run_sub_workflow`],
/// sharing the host [`Capabilities`](crate::caps::Capabilities) with the parent
/// run, and its final run state is emitted as an output item.
///
/// ## Execution: the multiplier
///
/// `config.execution` (default `once`) decides how many child runs this node
/// performs:
///
/// - `once` — one child run seeded with the node's whole input array.
/// - `per_item` — **one full child run per input item**, each seeded with just
///   that item and resolving `workflow_id` against it (so `=item.x` addresses
///   the element that run is for). `config.concurrency` bounds how many run at
///   a time and `config.on_item_error` what a failing child does to the batch
///   (see [`crate::nodes::map`]). This is how an array of work becomes N
///   parallel multi-step workflows.
///
/// Only the fields *this* node reads are `=`-resolved; an inline `workflow`
/// graph always passes through untouched because its expressions belong to the
/// child run.
///
/// The depth guard below is per child run, so a fan-out widens a run without
/// deepening it — N siblings at depth d+1, never d+N.
///
/// ## Passing the child's declared inputs
///
/// An optional `inputs` config object supplies values for the child's declared
/// [`WorkflowInput`](crate::model::WorkflowInput)s. Each field is resolved
/// against the parent's expression scope, so a parent can forward its own
/// inputs or an upstream node's output:
///
/// ```json
/// {
///   "workflow_id": "review-and-fix",
///   "inputs": { "repo": "=inputs.repo", "depth": 2 }
/// }
/// ```
///
/// The child validates what arrives against its own declarations, so a parent
/// that omits a required child input fails the same way a top-level caller
/// would — before the child executes anything.
///
/// Under `execution: "per_item"` the fields are resolved against **the current
/// element**, exactly like `workflow_id` is, so each child in a fan-out gets
/// values derived from its own item:
///
/// ```json
/// {
///   "workflow_id": "review-and-fix",
///   "execution": "per_item",
///   "inputs": { "repo": "=item.name" }
/// }
/// ```
///
/// ## Cycle / depth handling
///
/// Every nested `sub_workflow` run (inline or by id) increments a
/// `run.sub_workflow_depth` counter; a child that would exceed
/// [`MAX_SUB_WORKFLOW_DEPTH`] is refused. This bounds **any** cycle — including
/// indirect ones like flow A → flow B → flow A by id — after at most that many
/// levels. In addition, a **direct self-reference** (a resolved child graph that
/// itself references the same `workflow_id`) is caught statically here before the
/// child ever runs, so the common one-level loop fails fast with a clear error
/// rather than unwinding the full depth budget.
#[derive(Debug, Default, Clone)]
pub struct SubWorkflowNode;

/// Separates a `sub_workflow` node's id from a gate id inside its child.
///
/// Parent and child are separate graphs with separate id spaces, so a child's
/// gate `approve` and a parent's gate `approve` are different gates that would
/// otherwise be indistinguishable in one pending set.
const GATE_NAMESPACE: &str = "::";

/// Qualifies a child gate id with the node that ran the child.
fn namespaced_gate(node_id: &str, gate: &str) -> String {
    format!("{node_id}{GATE_NAMESPACE}{gate}")
}

/// The child-gate ids approved for `node_id`, taken from the parent run's
/// accumulated approvals with the namespace stripped.
///
/// This is how an approval crosses the boundary. `engine::resume` unions newly
/// approved ids into `run.trigger.approvals`, so on the re-run this node finds
/// the ones addressed to it and hands them to the child as *its* approvals.
/// Ids belonging to the parent or to a different `sub_workflow` node are left
/// alone.
fn approvals_for_child(ctx: &NodeContext<'_>) -> Vec<String> {
    let prefix = format!("{}{GATE_NAMESPACE}", ctx.node.id);
    let strip = |ids: &Value| -> Vec<String> {
        ids.as_array()
            .map(|ids| {
                ids.iter()
                    .filter_map(Value::as_str)
                    .filter_map(|id| id.strip_prefix(prefix.as_str()))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };

    // Two channels, because the engine has two resume paths and they deliver
    // approvals differently.
    //
    // `engine::resume` re-executes the workflow with the approvals merged into
    // the run input, so they arrive in `run.trigger.approvals`. The checkpointed
    // path replays from the checkpoint instead and hands the resume value
    // straight to the node that interrupted — this node — so they arrive in
    // `ctx.resume`. Reading only one of the two makes cross-boundary approval
    // work on one path and silently hang on the other.
    let mut approved: Vec<String> = ctx
        .run
        .get("trigger")
        .and_then(|trigger| trigger.get("approvals"))
        .map(&strip)
        .unwrap_or_default();
    if let Some(resume) = ctx.resume.as_ref().and_then(|value| value.get("approved")) {
        approved.extend(strip(resume));
    }
    approved.sort();
    approved.dedup();
    approved
}

/// Reads the current nesting depth from the run metadata (`0` at the top level).
fn current_depth(run: &Value) -> u64 {
    run.get("sub_workflow_depth")
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

/// The nesting cap in force for this run.
///
/// Seeded from the top-level graph's `trigger.config.max_sub_workflow_depth`
/// and forwarded down the chain by [`crate::engine::run_sub_workflow`], so
/// every level enforces the bound the *root* run declared rather than whatever
/// each child's own trigger happens to say. Falls back to
/// [`MAX_SUB_WORKFLOW_DEPTH`] when unset.
fn max_depth(run: &Value) -> u64 {
    run.get("max_sub_workflow_depth")
        .and_then(Value::as_u64)
        .filter(|n| *n > 0)
        .unwrap_or(MAX_SUB_WORKFLOW_DEPTH)
}

/// Deserializes the inline `workflow` config value into a [`WorkflowGraph`].
fn inline_child(workflow: &Value) -> Result<WorkflowGraph> {
    serde_json::from_value(workflow.clone())
        .map_err(|e| EngineError::Capability(format!("sub_workflow node: invalid workflow: {e}")))
}

/// Rejects a resolved child that references the same `workflow_id` it was loaded
/// under — a direct self-reference (one-level cycle). Deeper cycles are still
/// bounded by the depth counter; this catches the obvious case eagerly.
fn reject_self_reference(child: &WorkflowGraph, workflow_id: &str) -> Result<()> {
    let self_ref = child
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::SubWorkflow)
        .any(|n| n.config.get("workflow_id").and_then(Value::as_str) == Some(workflow_id));
    if self_ref {
        return Err(EngineError::Capability(format!(
            "sub_workflow node: workflow_id {workflow_id:?} references itself (cycle)"
        )));
    }
    Ok(())
}

/// Builds the values passed to the child's declared inputs from this node's
/// `inputs` config object.
///
/// Each field's value is resolved against the **parent's** scope, so a parent
/// can forward its own inputs (`"repo": "=inputs.repo"`), an upstream node's
/// output (`"=nodes.fetch.item.url"`), or a literal. The child then validates
/// what arrives against its own declarations, exactly as a top-level caller
/// would — a parent that forgets a required child input fails loudly.
///
/// An absent or non-object `inputs` config yields an empty map, so a
/// `sub_workflow` node authored before inputs existed keeps working against a
/// child that declares none.
fn child_inputs(config: &Value, scope: &Value) -> Result<serde_json::Map<String, Value>> {
    let Some(declared) = config.get("inputs") else {
        return Ok(serde_json::Map::new());
    };
    let Some(fields) = declared.as_object() else {
        return Err(EngineError::Capability(
            "sub_workflow node: `inputs` must be an object mapping the child's declared input \
             names to values"
                .to_string(),
        ));
    };
    Ok(fields
        .iter()
        .map(|(name, value)| (name.clone(), crate::expr::resolve(value, scope)))
        .collect())
}

mod execution;

#[cfg(test)]
#[path = "sub_workflow_tests.rs"]
mod tests;

/// Cross-boundary cancellation: a parent run's [`CancellationToken`] must reach
/// its `sub_workflow` children so a parent cancel winds the whole subtree down
/// instead of orphaning it behind a fresh token. These pin the propagation
/// end-to-end through a real `run_cancellable` drive (T1–T5).
///
/// The mid-flight cancel is made **deterministic under parallel test load** by
/// having the `slow` node hold the run at its boundary until the token actually
/// flips (a bounded spin), rather than racing a wall-clock sleep against the
/// scheduler — so the boundary check before `marker` is guaranteed to observe
/// the cancellation.
#[cfg(test)]
#[path = "cancellation_propagation_tests.rs"]
mod cancellation_propagation_tests;
