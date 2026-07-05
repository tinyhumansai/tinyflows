//! The `sub_workflow` node: runs another workflow as a nested sub-graph.

use async_trait::async_trait;
use serde_json::Value;

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
/// run, and its final run state is emitted as a single output item.
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

/// Reads the current nesting depth from the run metadata (`0` at the top level).
fn current_depth(run: &Value) -> u64 {
    run.get("sub_workflow_depth")
        .and_then(Value::as_u64)
        .unwrap_or(0)
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

#[async_trait]
impl NodeExecutor for SubWorkflowNode {
    async fn execute(&self, ctx: NodeContext<'_>) -> Result<NodeOutput> {
        let inline = ctx.node.config.get("workflow");
        let workflow_id = ctx
            .node
            .config
            .get("workflow_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());

        // Exactly one of `workflow` / `workflow_id` must be set.
        let child: WorkflowGraph = match (inline, workflow_id) {
            (Some(_), Some(_)) => {
                return Err(EngineError::Capability(
                    "sub_workflow node: set exactly one of `workflow` (inline) or `workflow_id` \
                     (reference), not both"
                        .to_string(),
                ));
            }
            (None, None) => {
                return Err(EngineError::Capability(
                    "sub_workflow node: missing `workflow` (inline) or `workflow_id` (reference) \
                     in config"
                        .to_string(),
                ));
            }
            (Some(inline_value), None) => {
                tracing::debug!(node = %ctx.node.id, "sub_workflow: running inline child graph");
                inline_child(inline_value)?
            }
            (None, Some(id)) => {
                tracing::debug!(node = %ctx.node.id, workflow_id = %id, "sub_workflow: resolving child graph by workflow_id");
                let resolved = ctx.caps.resolver.resolve(id).await?;
                reject_self_reference(&resolved, id)?;
                resolved
            }
        };

        // Depth / cycle guard: bound total nesting regardless of how a cycle is
        // formed. The child runs one level deeper than the current run.
        let child_depth = current_depth(ctx.run) + 1;
        if child_depth > MAX_SUB_WORKFLOW_DEPTH {
            return Err(EngineError::Capability(format!(
                "sub_workflow node: maximum nesting depth {MAX_SUB_WORKFLOW_DEPTH} exceeded \
                 (possible cycle)"
            )));
        }

        let compiled = crate::compiler::compile(&child)?;
        let input =
            serde_json::to_value(ctx.input).map_err(|e| EngineError::Capability(e.to_string()))?;
        // Box the recursive engine call so the async future type stays sized.
        let outcome = Box::pin(crate::engine::run_sub_workflow(
            &compiled,
            input,
            ctx.caps,
            child_depth,
        ))
        .await?;
        Ok(NodeOutput::main(vec![crate::data::Item::new(
            outcome.output,
        )]))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::SubWorkflowNode;
    use crate::caps::Capabilities;
    use crate::caps::mock::{
        MockWorkflowResolver, mock_capabilities, mock_capabilities_with_resolver,
    };
    use crate::compiler::compile;
    use crate::engine::run;
    use crate::error::EngineError;
    use crate::model::{Edge, Node, NodeKind, WorkflowGraph};
    use crate::nodes::{NodeContext, NodeExecutor, NodeOutput};

    fn node(id: &str, kind: NodeKind) -> Node {
        Node {
            id: id.to_string(),
            kind,
            type_version: 1,
            name: id.to_string(),
            config: Value::Null,
            ports: Vec::new(),
            position: None,
        }
    }

    async fn execute_err(config: Value) -> EngineError {
        let mut sw = node("sw", NodeKind::SubWorkflow);
        sw.config = config;
        let input = vec![];
        let caps = mock_capabilities();
        let run_meta = Value::Null;
        let ctx = NodeContext {
            node: &sw,
            input: &input,
            run: &run_meta,
            caps: &caps,
        };
        SubWorkflowNode
            .execute(ctx)
            .await
            .expect_err("expected an error")
    }

    #[tokio::test]
    async fn missing_workflow_config_is_a_capability_error() {
        let err = execute_err(Value::Null).await;
        assert!(
            matches!(err, EngineError::Capability(ref m) if m.contains("workflow")),
            "expected a capability error mentioning `workflow`, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn invalid_workflow_value_is_a_capability_error() {
        // A non-graph value under `workflow` fails to deserialize into a graph.
        let err = execute_err(json!({ "workflow": 123 })).await;
        assert!(
            matches!(err, EngineError::Capability(ref m) if m.contains("invalid workflow")),
            "expected a capability error about an invalid workflow, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn sub_workflow_runs_embedded_child_graph() {
        // The child is a single trigger node; serialize it into the parent's
        // sub_workflow config so the executor compiles and runs it.
        let child = WorkflowGraph {
            nodes: vec![node("ct", NodeKind::Trigger)],
            ..Default::default()
        };
        let child_value = serde_json::to_value(&child).expect("serialize child");

        let mut sw = node("sw", NodeKind::SubWorkflow);
        sw.config = json!({ "workflow": child_value });

        let parent = WorkflowGraph {
            nodes: vec![node("t", NodeKind::Trigger), sw],
            edges: vec![Edge {
                from_node: "t".to_string(),
                from_port: "main".to_string(),
                to_node: "sw".to_string(),
                to_port: "main".to_string(),
            }],
            ..Default::default()
        };

        let compiled = compile(&parent).expect("compile parent");
        let caps = mock_capabilities();

        let out = run(&compiled, json!({ "hi": 1 }), &caps)
            .await
            .expect("run parent");

        // The sub_workflow emits the child's final run state as its single item.
        // The child seeds its trigger from the input the parent passed, which is
        // the serialized parent items delivered to the sub_workflow node — an
        // array of `Item`s — so the child's `run.trigger` is that array.
        let child_state = &out.output["nodes"]["sw"]["items"][0]["json"];
        assert_eq!(
            child_state["run"]["trigger"],
            json!([{ "json": { "hi": 1 } }]),
            "child trigger should be seeded with the parent's serialized items"
        );
        // And the child actually ran: its trigger node recorded that same payload.
        assert_eq!(
            child_state["nodes"]["ct"]["items"][0]["json"],
            json!([{ "json": { "hi": 1 } }]),
            "child trigger node should have run and echoed its seeded input"
        );
    }

    /// Executes a lone `sub_workflow` node with the given config, run metadata,
    /// and capabilities, returning its raw [`Result`].
    async fn execute_with(
        config: Value,
        run_meta: Value,
        caps: &Capabilities,
    ) -> Result<NodeOutput, EngineError> {
        let mut sw = node("sw", NodeKind::SubWorkflow);
        sw.config = config;
        let input = vec![];
        let ctx = NodeContext {
            node: &sw,
            input: &input,
            run: &run_meta,
            caps,
        };
        SubWorkflowNode.execute(ctx).await
    }

    #[tokio::test]
    async fn both_workflow_and_workflow_id_is_rejected() {
        // Exactly one of the two config keys may be set.
        let err = execute_err(json!({
            "workflow": { "nodes": [], "edges": [] },
            "workflow_id": "child-1"
        }))
        .await;
        assert!(
            matches!(err, EngineError::Capability(ref m) if m.contains("exactly one")),
            "expected an exactly-one config error, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn empty_workflow_id_falls_back_to_missing_config_error() {
        // A blank `workflow_id` is treated as absent, so with no inline
        // `workflow` either the node reports the missing-config error.
        let err = execute_err(json!({ "workflow_id": "" })).await;
        assert!(
            matches!(err, EngineError::Capability(ref m) if m.contains("missing")),
            "expected a missing-config error, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn sub_workflow_by_id_resolves_via_resolver_and_executes() {
        // The saved child is a single trigger node, registered under an id the
        // parent references via `workflow_id`.
        let child = WorkflowGraph {
            nodes: vec![node("ct", NodeKind::Trigger)],
            ..Default::default()
        };
        let caps =
            mock_capabilities_with_resolver(MockWorkflowResolver::default().with("child-1", child));

        let mut sw = node("sw", NodeKind::SubWorkflow);
        sw.config = json!({ "workflow_id": "child-1" });
        let parent = WorkflowGraph {
            nodes: vec![node("t", NodeKind::Trigger), sw],
            edges: vec![Edge {
                from_node: "t".to_string(),
                from_port: "main".to_string(),
                to_node: "sw".to_string(),
                to_port: "main".to_string(),
            }],
            ..Default::default()
        };
        let compiled = compile(&parent).expect("compile parent");

        let out = run(&compiled, json!({ "hi": 1 }), &caps)
            .await
            .expect("run parent");

        // The referenced child was resolved and actually ran.
        let child_state = &out.output["nodes"]["sw"]["items"][0]["json"];
        assert_eq!(
            child_state["nodes"]["ct"]["items"][0]["json"],
            json!([{ "json": { "hi": 1 } }]),
            "resolved child trigger node should have run and echoed its seeded input"
        );
        // The child ran one nesting level deep.
        assert_eq!(child_state["run"]["sub_workflow_depth"], json!(1));
    }

    #[tokio::test]
    async fn unknown_workflow_id_surfaces_resolver_error() {
        // The default mock resolver knows no ids, so resolution fails.
        let caps = mock_capabilities();
        let err = execute_with(json!({ "workflow_id": "nope" }), Value::Null, &caps)
            .await
            .expect_err("unknown id must error");
        assert!(
            matches!(err, EngineError::Capability(ref m) if m.contains("nope")),
            "expected the resolver's unknown-id error, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn direct_self_reference_by_id_is_rejected() {
        // The saved child itself references the same id — a one-level cycle,
        // caught statically before it runs.
        let mut inner = node("inner", NodeKind::SubWorkflow);
        inner.config = json!({ "workflow_id": "loop-1" });
        let child = WorkflowGraph {
            nodes: vec![node("ct", NodeKind::Trigger), inner],
            ..Default::default()
        };
        let caps =
            mock_capabilities_with_resolver(MockWorkflowResolver::default().with("loop-1", child));

        let err = execute_with(json!({ "workflow_id": "loop-1" }), Value::Null, &caps)
            .await
            .expect_err("self-reference must be rejected");
        assert!(
            matches!(err, EngineError::Capability(ref m) if m.contains("cycle")),
            "expected a cycle rejection, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn depth_limit_is_enforced() {
        // A run already at the maximum nesting depth refuses to descend further,
        // even for a trivial resolvable child (bounds indirect cycles).
        let child = WorkflowGraph {
            nodes: vec![node("ct", NodeKind::Trigger)],
            ..Default::default()
        };
        let caps =
            mock_capabilities_with_resolver(MockWorkflowResolver::default().with("child-1", child));

        let run_meta = json!({ "sub_workflow_depth": crate::engine::MAX_SUB_WORKFLOW_DEPTH });
        let err = execute_with(json!({ "workflow_id": "child-1" }), run_meta, &caps)
            .await
            .expect_err("exceeding the depth budget must error");
        assert!(
            matches!(err, EngineError::Capability(ref m) if m.contains("depth")),
            "expected a depth-limit error, got: {err:?}"
        );
    }
}
