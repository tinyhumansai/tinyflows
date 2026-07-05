//! The `split_out` node: per-item fan-out.

use async_trait::async_trait;

use crate::data::Item;
use crate::error::Result;
use crate::nodes::{NodeContext, NodeExecutor, NodeOutput};

/// Fan-out that emits one item per element of a list.
#[derive(Debug, Default, Clone)]
pub struct SplitOutNode;

#[async_trait]
impl NodeExecutor for SplitOutNode {
    async fn execute(&self, ctx: NodeContext<'_>) -> Result<NodeOutput> {
        let path = ctx
            .node
            .config
            .get("path")
            .and_then(serde_json::Value::as_str);
        let mut out = Vec::new();
        for (index, item) in ctx.input.iter().enumerate() {
            let value = match path {
                Some(p) if !p.is_empty() => {
                    item.json.get(p).cloned().unwrap_or(serde_json::Value::Null)
                }
                _ => item.json.clone(),
            };
            match value {
                serde_json::Value::Array(elements) => {
                    for element in elements {
                        out.push(Item::new(element).paired_with(index));
                    }
                }
                other => out.push(Item::new(other).paired_with(index)),
            }
        }
        Ok(NodeOutput::main(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::mock::mock_capabilities;
    use crate::model::{Node, NodeKind};
    use serde_json::{Value, json};

    fn split_out_node(config: Value) -> Node {
        Node {
            id: "s".to_string(),
            kind: NodeKind::SplitOut,
            type_version: 1,
            name: "s".to_string(),
            config,
            ports: Vec::new(),
            position: None,
        }
    }

    #[tokio::test]
    async fn fans_out_array_at_path() {
        let node = split_out_node(json!({ "path": "items" }));
        let input = vec![Item::new(json!({ "items": [1, 2, 3] }))];
        let caps = mock_capabilities();
        let ctx = NodeContext {
            node: &node,
            input: &input,
            run: &Value::Null,
            nodes: &Value::Null,
            caps: &caps,
        };

        let output = SplitOutNode.execute(ctx).await.expect("execute");

        assert_eq!(output.items.len(), 3);
        for (i, expected) in [1, 2, 3].into_iter().enumerate() {
            assert_eq!(output.items[i].json, json!(expected));
            assert_eq!(output.items[i].paired_item, Some(0));
        }
    }

    #[tokio::test]
    async fn missing_path_uses_whole_json() {
        let node = split_out_node(json!({ "path": "" }));
        let input = vec![Item::new(json!([10, 20]))];
        let caps = mock_capabilities();
        let ctx = NodeContext {
            node: &node,
            input: &input,
            run: &Value::Null,
            nodes: &Value::Null,
            caps: &caps,
        };

        let output = SplitOutNode.execute(ctx).await.expect("execute");

        assert_eq!(output.items.len(), 2);
        assert_eq!(output.items[0].json, json!(10));
        assert_eq!(output.items[1].json, json!(20));
        assert_eq!(output.items[1].paired_item, Some(0));
    }

    #[tokio::test]
    async fn non_array_value_emits_single_item() {
        let node = split_out_node(json!({ "path": "value" }));
        let input = vec![Item::new(json!({ "value": "hello" }))];
        let caps = mock_capabilities();
        let ctx = NodeContext {
            node: &node,
            input: &input,
            run: &Value::Null,
            nodes: &Value::Null,
            caps: &caps,
        };

        let output = SplitOutNode.execute(ctx).await.expect("execute");

        assert_eq!(output.items.len(), 1);
        assert_eq!(output.items[0].json, json!("hello"));
        assert_eq!(output.items[0].paired_item, Some(0));
    }

    async fn run_split(config: Value, input: Vec<Item>) -> Vec<Item> {
        let node = split_out_node(config);
        let caps = mock_capabilities();
        let ctx = NodeContext {
            node: &node,
            input: &input,
            run: &Value::Null,
            nodes: &Value::Null,
            caps: &caps,
        };
        SplitOutNode.execute(ctx).await.expect("execute").items
    }

    #[tokio::test]
    async fn missing_path_key_emits_single_null_item() {
        // The configured path names a key that is absent → resolves to `null`,
        // which is non-array → one item carrying `null`, paired to input 0.
        let out = run_split(
            json!({ "path": "nope" }),
            vec![Item::new(json!({ "items": [1, 2] }))],
        )
        .await;
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].json, Value::Null);
        assert_eq!(out[0].paired_item, Some(0));
    }

    #[tokio::test]
    async fn empty_array_emits_no_items() {
        let out = run_split(
            json!({ "path": "items" }),
            vec![Item::new(json!({ "items": [] }))],
        )
        .await;
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn multiple_inputs_preserve_pairing_index() {
        let input = vec![
            Item::new(json!({ "items": [1, 2] })),
            Item::new(json!({ "items": [3] })),
        ];
        let out = run_split(json!({ "path": "items" }), input).await;
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].json, json!(1));
        assert_eq!(out[0].paired_item, Some(0));
        assert_eq!(out[1].json, json!(2));
        assert_eq!(out[1].paired_item, Some(0));
        assert_eq!(out[2].json, json!(3));
        assert_eq!(
            out[2].paired_item,
            Some(1),
            "the second input's element pairs to index 1"
        );
    }

    #[tokio::test]
    async fn empty_input_emits_no_items() {
        assert!(
            run_split(json!({ "path": "items" }), vec![])
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn drives_end_to_end_via_engine() {
        use crate::compiler::compile;
        use crate::model::{Edge, WorkflowGraph};

        let trigger = Node {
            id: "t".to_string(),
            kind: NodeKind::Trigger,
            type_version: 1,
            name: "t".to_string(),
            config: Value::Null,
            ports: Vec::new(),
            position: None,
        };
        let graph = WorkflowGraph {
            nodes: vec![trigger, split_out_node(json!({ "path": "items" }))],
            edges: vec![Edge {
                from_node: "t".to_string(),
                from_port: "main".to_string(),
                to_node: "s".to_string(),
                to_port: "main".to_string(),
            }],
            ..Default::default()
        };
        let compiled = compile(&graph).expect("compile");
        let caps = mock_capabilities();

        let outcome = crate::engine::run(&compiled, json!({ "items": [1, 2, 3] }), &caps)
            .await
            .expect("run");

        let items = &outcome.output["nodes"]["s"]["items"];
        assert_eq!(items[0]["json"], json!(1));
        assert_eq!(items[1]["json"], json!(2));
        assert_eq!(items[2]["json"], json!(3));
    }
}
