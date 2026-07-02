//! The `sub_workflow` node: runs another workflow as a nested sub-graph.

use async_trait::async_trait;

use crate::error::{EngineError, Result};
use crate::nodes::{NodeContext, NodeExecutor, NodeOutput};

/// Runs another workflow as a nested sub-graph.
///
/// The child [`WorkflowGraph`](crate::model::WorkflowGraph) is embedded under the
/// node's `workflow` config key. The node compiles and runs that child graph via
/// the same [`crate::engine::run`], sharing the host [`Capabilities`](crate::caps::Capabilities)
/// with the parent run, and emits the child's final run state as a single output item.
#[derive(Debug, Default, Clone)]
pub struct SubWorkflowNode;

#[async_trait]
impl NodeExecutor for SubWorkflowNode {
    async fn execute(&self, ctx: NodeContext<'_>) -> Result<NodeOutput> {
        let child_value = ctx.node.config.get("workflow").ok_or_else(|| {
            EngineError::Capability("sub_workflow node: missing `workflow` in config".to_string())
        })?;
        let child: crate::model::WorkflowGraph = serde_json::from_value(child_value.clone())
            .map_err(|e| {
                EngineError::Capability(format!("sub_workflow node: invalid workflow: {e}"))
            })?;
        let compiled = crate::compiler::compile(&child)?;
        let input =
            serde_json::to_value(ctx.input).map_err(|e| EngineError::Capability(e.to_string()))?;
        // Box the recursive engine call so the async future type stays sized.
        let outcome = Box::pin(crate::engine::run(&compiled, input, ctx.caps)).await?;
        Ok(NodeOutput::main(vec![crate::data::Item::new(
            outcome.output,
        )]))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use crate::caps::mock::mock_capabilities;
    use crate::compiler::compile;
    use crate::engine::run;
    use crate::model::{Edge, Node, NodeKind, WorkflowGraph};

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
}
