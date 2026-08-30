use super::*;
use crate::caps::mock::mock_capabilities;
use crate::data::Item;
use crate::model::{Node, NodeKind};
use serde_json::{Value, json};

fn merge_node() -> Node {
    Node {
        id: "m".to_string(),
        kind: NodeKind::Merge,
        type_version: 1,
        name: "m".to_string(),
        config: Value::Null,
        ports: Vec::new(),
        position: None,
    }
}

#[tokio::test]
async fn passes_through_concatenated_input() {
    let node = merge_node();
    let input = vec![Item::new(json!({ "a": 1 })), Item::new(json!({ "b": 2 }))];
    let caps = mock_capabilities();
    let ctx = NodeContext {
        node: &node,
        input: &input,
        run: &Value::Null,
        nodes: &Value::Null,
        caps: &caps,
        agents: &[],
        observer: &crate::observability::NoopObserver,
        token: crate::engine::CancellationToken::new(),
        lane: None,
        resume: None,
        step: 0,
    };

    let output = MergeNode.execute(ctx).await.expect("execute");

    assert_eq!(output.items.len(), 2);
    assert_eq!(output.items, input);
    assert_eq!(output.port, None, "merge emits on the default main port");
}

async fn run_merge(input: Vec<Item>) -> Vec<Item> {
    let node = merge_node();
    let caps = mock_capabilities();
    let ctx = NodeContext {
        node: &node,
        input: &input,
        run: &Value::Null,
        nodes: &Value::Null,
        caps: &caps,
        agents: &[],
        observer: &crate::observability::NoopObserver,
        token: crate::engine::CancellationToken::new(),
        lane: None,
        resume: None,
        step: 0,
    };
    MergeNode.execute(ctx).await.expect("execute").items
}

#[tokio::test]
async fn single_input_passes_through() {
    let input = vec![Item::new(json!({ "only": 1 }))];
    assert_eq!(run_merge(input.clone()).await, input);
}

#[tokio::test]
async fn empty_input_yields_no_items() {
    assert!(run_merge(vec![]).await.is_empty());
}
