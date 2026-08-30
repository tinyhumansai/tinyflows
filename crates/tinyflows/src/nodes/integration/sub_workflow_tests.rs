use serde_json::{Value, json};

use super::SubWorkflowNode;
use crate::caps::Capabilities;
use crate::caps::mock::{MockWorkflowResolver, mock_capabilities, mock_capabilities_with_resolver};
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
        nodes: &Value::Null,
        caps: &caps,
        agents: &[],
        observer: &crate::observability::NoopObserver,
        token: crate::engine::CancellationToken::new(),
        lane: None,
        resume: None,
        step: 0,
    };
    SubWorkflowNode
        .execute(ctx)
        .await
        .expect_err("expected an error")
}

/// Runs a `sub_workflow` node with the given config over `input_items`.
async fn execute_over(
    config: Value,
    input_items: Vec<crate::data::Item>,
    caps: &Capabilities,
) -> NodeOutput {
    let mut sw = node("sw", NodeKind::SubWorkflow);
    sw.config = config;
    let run_meta = Value::Null;
    let ctx = NodeContext {
        node: &sw,
        input: &input_items,
        run: &run_meta,
        nodes: &Value::Null,
        caps,
        agents: &[],
        observer: &crate::observability::NoopObserver,
        token: crate::engine::CancellationToken::new(),
        lane: None,
        resume: None,
        step: 0,
    };
    SubWorkflowNode.execute(ctx).await.expect("execute")
}

/// A child graph whose trigger simply carries the payload it was seeded with.
fn passthrough_child() -> WorkflowGraph {
    WorkflowGraph {
        nodes: vec![node("ct", NodeKind::Trigger)],
        ..Default::default()
    }
}

include!("sub_workflow_tests/sub_workflow_tests_part_01_tests.rs");
