//! The `condition` node: a two-way IF branch.

use async_trait::async_trait;

use crate::error::Result;
use crate::nodes::{NodeContext, NodeExecutor, NodeOutput};

/// Two-way conditional branch, emitting on the `true` or `false` port.
#[derive(Debug, Default, Clone)]
pub struct ConditionNode;

#[async_trait]
impl NodeExecutor for ConditionNode {
    async fn execute(&self, ctx: NodeContext<'_>) -> Result<NodeOutput> {
        let field = ctx
            .node
            .config
            .get("field")
            .and_then(serde_json::Value::as_str);
        let truthy = ctx.input.first().is_some_and(|item| {
            let value = match field {
                Some(f) => item.json.get(f).unwrap_or(&serde_json::Value::Null),
                None => &item.json,
            };
            is_truthy(value)
        });
        Ok(NodeOutput::routed(
            ctx.input.to_vec(),
            if truthy { "true" } else { "false" },
        ))
    }
}

/// Truthiness predicate: `null`, `false`, `0`, `""`, `[]`, and `{}` are falsey;
/// every other value is truthy.
fn is_truthy(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Null => false,
        serde_json::Value::Bool(b) => *b,
        serde_json::Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        serde_json::Value::String(s) => !s.is_empty(),
        serde_json::Value::Array(a) => !a.is_empty(),
        serde_json::Value::Object(o) => !o.is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::mock::mock_capabilities;
    use crate::data::Item;
    use crate::model::{Node, NodeKind};
    use serde_json::{Value, json};

    fn cond_node(config: Value) -> Node {
        Node {
            id: "c".to_string(),
            kind: NodeKind::Condition,
            type_version: 1,
            name: "c".to_string(),
            config,
            ports: Vec::new(),
            position: None,
        }
    }

    /// Executes the condition node and returns `(routed_port, emitted_items)`.
    async fn route(config: Value, input: Vec<Item>) -> (String, Vec<Item>) {
        let node = cond_node(config);
        let caps = mock_capabilities();
        let run = Value::Null;
        let ctx = NodeContext {
            node: &node,
            input: &input,
            run: &run,
            caps: &caps,
        };
        let out = ConditionNode.execute(ctx).await.expect("execute");
        (
            out.port.expect("condition always routes to a port"),
            out.items,
        )
    }

    #[test]
    fn is_truthy_classifies_every_json_kind() {
        for falsey in [
            json!(null),
            json!(false),
            json!(0),
            json!(0.0),
            json!(""),
            json!([]),
            json!({}),
        ] {
            assert!(!is_truthy(&falsey), "{falsey:?} should be falsey");
        }
        for truthy in [
            json!(true),
            json!(1),
            json!(-1),
            json!(1.5),
            json!("x"),
            json!([0]),
            json!({ "k": 1 }),
        ] {
            assert!(is_truthy(&truthy), "{truthy:?} should be truthy");
        }
    }

    #[tokio::test]
    async fn falsey_field_values_route_false() {
        for v in [
            json!(null),
            json!(false),
            json!(0),
            json!(""),
            json!([]),
            json!({}),
        ] {
            let (port, _) = route(
                json!({ "field": "f" }),
                vec![Item::new(json!({ "f": v.clone() }))],
            )
            .await;
            assert_eq!(port, "false", "field value {v:?} should route false");
        }
    }

    #[tokio::test]
    async fn truthy_field_values_route_true() {
        for v in [
            json!(true),
            json!(1),
            json!(-5),
            json!(2.5),
            json!("hello"),
            json!([0]),
            json!({ "k": 1 }),
        ] {
            let (port, _) = route(
                json!({ "field": "f" }),
                vec![Item::new(json!({ "f": v.clone() }))],
            )
            .await;
            assert_eq!(port, "true", "field value {v:?} should route true");
        }
    }

    #[tokio::test]
    async fn missing_field_key_routes_false() {
        // The configured field is absent on the item → treated as `null` → false.
        let (port, _) = route(
            json!({ "field": "absent" }),
            vec![Item::new(json!({ "f": true }))],
        )
        .await;
        assert_eq!(port, "false");
    }

    #[tokio::test]
    async fn no_field_config_tests_the_whole_item() {
        // Without a `field`, the whole item JSON is the truthiness subject.
        let (truthy, _) = route(Value::Null, vec![Item::new(json!({ "a": 1 }))]).await;
        assert_eq!(truthy, "true");
        let (falsey, _) = route(Value::Null, vec![Item::new(json!({}))]).await;
        assert_eq!(falsey, "false");
    }

    #[tokio::test]
    async fn empty_input_routes_false_with_no_items() {
        let (port, items) = route(json!({ "field": "f" }), vec![]).await;
        assert_eq!(port, "false");
        assert!(items.is_empty());
    }

    #[tokio::test]
    async fn only_first_item_decides_but_all_items_pass_through() {
        // Truthiness keys off the first item; every input item is forwarded.
        let input = vec![
            Item::new(json!({ "f": true })),
            Item::new(json!({ "f": false })),
        ];
        let (port, items) = route(json!({ "field": "f" }), input.clone()).await;
        assert_eq!(port, "true", "first item decides the branch");
        assert_eq!(items, input, "all input items are routed through unchanged");
    }
}
