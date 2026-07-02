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
    use crate::caps::mock::mock_capabilities;
    use crate::compiler::compile;
    use crate::model::{Edge, Node, NodeKind, WorkflowGraph};
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
