//! The `transform` node: a pure, expression-based data transform.

use async_trait::async_trait;

use crate::data::Item;
use crate::error::Result;
use crate::nodes::{NodeContext, NodeExecutor, NodeOutput};

/// Pure, expression-based data transform over the run state.
#[derive(Debug, Default, Clone)]
pub struct TransformNode;

#[async_trait]
impl NodeExecutor for TransformNode {
    async fn execute(&self, ctx: NodeContext<'_>) -> Result<NodeOutput> {
        let set = ctx
            .node
            .config
            .get("set")
            .and_then(serde_json::Value::as_object)
            .cloned();
        let mut out = Vec::with_capacity(ctx.input.len());
        for (index, item) in ctx.input.iter().enumerate() {
            let scope = serde_json::json!({ "item": item.json.clone(), "run": ctx.run });
            let mut json = item.json.clone();
            if let Some(set) = &set {
                if !json.is_object() {
                    json = serde_json::Value::Object(serde_json::Map::new());
                }
                if let Some(obj) = json.as_object_mut() {
                    for (key, expr) in set {
                        obj.insert(key.clone(), crate::expr::evaluate(expr, &scope));
                    }
                }
            }
            out.push(Item::new(json).paired_with(index));
        }
        Ok(NodeOutput::main(out))
    }
}

#[cfg(test)]
mod tests {
    use super::TransformNode;
    use crate::caps::mock::mock_capabilities;
    use crate::compiler::compile;
    use crate::data::Item;
    use crate::model::{Edge, Node, NodeKind, WorkflowGraph};
    use crate::nodes::{NodeContext, NodeExecutor};
    use serde_json::{Value, json};

    fn node(id: &str, kind: NodeKind, config: Value) -> Node {
        Node {
            id: id.to_string(),
            kind,
            type_version: 1,
            name: id.to_string(),
            config,
            ports: Vec::new(),
            position: None,
        }
    }

    /// Executes the transform node directly against the given input.
    async fn run_transform(config: Value, input: Vec<Item>) -> Vec<Item> {
        let node = node("n", NodeKind::Transform, config);
        let caps = mock_capabilities();
        let run = Value::Null;
        let ctx = NodeContext {
            node: &node,
            input: &input,
            run: &run,
            caps: &caps,
        };
        TransformNode.execute(ctx).await.expect("execute").items
    }

    #[tokio::test]
    async fn set_inserts_literal_values() {
        let out = run_transform(
            json!({ "set": { "greeting": "hello" } }),
            vec![Item::new(json!({}))],
        )
        .await;
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].json, json!({ "greeting": "hello" }));
        assert_eq!(out[0].paired_item, Some(0));
    }

    #[tokio::test]
    async fn set_evaluates_dotted_expression() {
        let out = run_transform(
            json!({ "set": { "g": "=item.name" } }),
            vec![Item::new(json!({ "name": "Ada" }))],
        )
        .await;
        assert_eq!(out[0].json["g"], json!("Ada"));
        assert_eq!(
            out[0].json["name"],
            json!("Ada"),
            "existing fields are preserved"
        );
    }

    #[tokio::test]
    async fn set_evaluates_jaq_expression() {
        let out = run_transform(
            json!({ "set": { "n": "=.item.items | length" } }),
            vec![Item::new(json!({ "items": [1, 2, 3, 4] }))],
        )
        .await;
        assert_eq!(out[0].json["n"], json!(4));
    }

    #[tokio::test]
    async fn non_object_input_becomes_an_object_of_the_set_keys() {
        // A scalar item can't hold keys, so it is replaced by a fresh object
        // carrying only the `set` results; the original scalar is dropped.
        let out = run_transform(
            json!({ "set": { "tag": "fixed" } }),
            vec![Item::new(json!(5))],
        )
        .await;
        assert_eq!(out[0].json, json!({ "tag": "fixed" }));
    }

    #[tokio::test]
    async fn empty_set_leaves_the_item_unchanged() {
        let out = run_transform(json!({ "set": {} }), vec![Item::new(json!({ "a": 1 }))]).await;
        assert_eq!(out[0].json, json!({ "a": 1 }));
        assert_eq!(out[0].paired_item, Some(0));
    }

    #[tokio::test]
    async fn absent_set_config_is_a_passthrough() {
        let out = run_transform(Value::Null, vec![Item::new(json!({ "a": 1 }))]).await;
        assert_eq!(out[0].json, json!({ "a": 1 }));
        assert_eq!(out[0].paired_item, Some(0));
    }

    #[tokio::test]
    async fn multiple_items_keep_pairing() {
        let input = vec![Item::new(json!({ "n": 1 })), Item::new(json!({ "n": 2 }))];
        let out = run_transform(json!({ "set": { "doubled": "=item.n" } }), input).await;
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].json["doubled"], json!(1));
        assert_eq!(out[0].paired_item, Some(0));
        assert_eq!(out[1].json["doubled"], json!(2));
        assert_eq!(out[1].paired_item, Some(1));
    }

    #[tokio::test]
    async fn overwriting_a_key_reads_from_the_original_scope() {
        // `set.x` overwrites the existing `x`, but the expression scope sees the
        // pre-transform item, so `=item.n` reads the original `n`.
        let out = run_transform(
            json!({ "set": { "x": "=item.n" } }),
            vec![Item::new(json!({ "x": 1, "n": 9 }))],
        )
        .await;
        assert_eq!(out[0].json, json!({ "x": 9, "n": 9 }));
    }

    #[tokio::test]
    async fn maps_fields_end_to_end_via_engine() {
        let trigger = node("t", NodeKind::Trigger, Value::Null);
        let transform = node(
            "n",
            NodeKind::Transform,
            json!({ "set": { "greeting": "=item.name" } }),
        );
        let graph = WorkflowGraph {
            nodes: vec![trigger, transform],
            edges: vec![Edge {
                from_node: "t".to_string(),
                from_port: "main".to_string(),
                to_node: "n".to_string(),
                to_port: "main".to_string(),
            }],
            ..Default::default()
        };
        let compiled = compile(&graph).expect("compile");
        let caps = mock_capabilities();

        let outcome = crate::engine::run(&compiled, json!({ "name": "Ada" }), &caps)
            .await
            .expect("run");

        let item = &outcome.output["nodes"]["n"]["items"][0]["json"];
        assert_eq!(item["greeting"], json!("Ada"));
        // The original field is preserved alongside the new one.
        assert_eq!(item["name"], json!("Ada"));
    }
}
