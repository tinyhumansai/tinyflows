//! The `switch` node: a multi-way branch.

use async_trait::async_trait;

use crate::error::Result;
use crate::nodes::{NodeContext, NodeExecutor, NodeOutput};

/// Multi-way branch keyed by a computed case value.
///
/// The case key comes from config: an `expression` (an `=`-expression evaluated
/// against the `{ item, items, run, nodes }` node scope) takes precedence,
/// otherwise a `field` names a key on the first input item. The resulting value
/// selects the output port to emit on, routing to the matching case; a `null`
/// result routes to the `default` port.
#[derive(Debug, Default, Clone)]
pub struct SwitchNode;

#[async_trait]
impl NodeExecutor for SwitchNode {
    async fn execute(&self, ctx: NodeContext<'_>) -> Result<NodeOutput> {
        // Use the shared node scope so `expression` can address upstream nodes by
        // id (`=nodes.<id>.item.<field>`) — same `{ item, items, run, nodes }`
        // scope the integration nodes get. `item` is the first input item.
        let scope = crate::nodes::expr_scope(&ctx);
        let value = if let Some(expr) = ctx.node.config.get("expression") {
            crate::expr::evaluate(expr, &scope)
        } else if let Some(field) = ctx
            .node
            .config
            .get("field")
            .and_then(serde_json::Value::as_str)
        {
            scope["item"]
                .get(field)
                .cloned()
                .unwrap_or(serde_json::Value::Null)
        } else {
            serde_json::Value::Null
        };
        // Map the discriminant to a port name. Only scalar values name a port
        // sensibly: a string is used verbatim, and a number/bool uses its natural
        // rendering (`42`, `true`) so switching on a numeric/boolean field works
        // predictably. A `null` or a non-scalar (object/array) has no meaningful
        // port name — dumping its JSON as a port would never match a real port and
        // is a confusing footgun — so those route to the `default` fallback port.
        let port = match value {
            serde_json::Value::String(s) => s,
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::Bool(b) => b.to_string(),
            serde_json::Value::Null
            | serde_json::Value::Object(_)
            | serde_json::Value::Array(_) => "default".to_string(),
        };
        Ok(NodeOutput::routed(ctx.input.to_vec(), port))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::SwitchNode;
    use crate::caps::mock::mock_capabilities;
    use crate::compiler::compile;
    use crate::data::Item;
    use crate::engine::run;
    use crate::model::{Edge, Node, NodeKind, WorkflowGraph};
    use crate::nodes::{NodeContext, NodeExecutor};

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

    /// Executes the switch node directly and returns `(routed_port, items)`.
    async fn route(config: Value, input: Vec<Item>) -> (String, Vec<Item>) {
        let mut sw = node("sw", NodeKind::Switch);
        sw.config = config;
        let caps = mock_capabilities();
        let run = Value::Null;
        let ctx = NodeContext {
            node: &sw,
            input: &input,
            run: &run,
            nodes: &Value::Null,
            caps: &caps,
        };
        let out = SwitchNode.execute(ctx).await.expect("execute");
        (out.port.expect("switch always routes to a port"), out.items)
    }

    #[tokio::test]
    async fn expression_can_reference_upstream_node_by_id() {
        // Regression (audit BUG-2): the switch `expression` scope must expose the
        // `nodes` map so it can branch on a completed upstream node's output.
        let mut sw = node("sw", NodeKind::Switch);
        sw.config = json!({ "expression": "=nodes.classify.item.label" });
        let caps = mock_capabilities();
        let run = Value::Null;
        let nodes = json!({ "classify": { "items": [{ "json": { "label": "urgent" } }] } });
        let input = vec![Item::new(json!({}))];
        let ctx = NodeContext {
            node: &sw,
            input: &input,
            run: &run,
            nodes: &nodes,
            caps: &caps,
        };
        let out = SwitchNode.execute(ctx).await.expect("execute");
        assert_eq!(out.port.as_deref(), Some("urgent"));
    }

    #[tokio::test]
    async fn field_string_value_selects_port() {
        let (port, _) = route(
            json!({ "field": "type" }),
            vec![Item::new(json!({ "type": "a" }))],
        )
        .await;
        assert_eq!(port, "a");
    }

    #[tokio::test]
    async fn expression_value_selects_port() {
        // A dotted-path expression over `{ item, run }`.
        let (port, _) = route(
            json!({ "expression": "=item.type" }),
            vec![Item::new(json!({ "type": "b" }))],
        )
        .await;
        assert_eq!(port, "b");
    }

    #[tokio::test]
    async fn expression_takes_precedence_over_field() {
        // Both keys present: the expression wins.
        let (port, _) = route(
            json!({ "expression": "=item.wanted", "field": "ignored" }),
            vec![Item::new(json!({ "wanted": "x", "ignored": "y" }))],
        )
        .await;
        assert_eq!(port, "x");
    }

    #[tokio::test]
    async fn jq_expression_selects_port() {
        // A non-dotted jq program is run by the jaq engine; its numeric output is
        // stringified to name the port.
        let (port, _) = route(
            json!({ "expression": "=.item.items | length" }),
            vec![Item::new(json!({ "items": [1, 2, 3] }))],
        )
        .await;
        assert_eq!(port, "3");
    }

    #[tokio::test]
    async fn numeric_discriminant_is_stringified() {
        let (port, _) = route(json!({ "field": "n" }), vec![Item::new(json!({ "n": 5 }))]).await;
        assert_eq!(port, "5");
    }

    #[tokio::test]
    async fn boolean_discriminant_is_stringified() {
        let (port, _) = route(
            json!({ "field": "b" }),
            vec![Item::new(json!({ "b": true }))],
        )
        .await;
        assert_eq!(port, "true");
    }

    #[tokio::test]
    async fn object_discriminant_routes_default() {
        // A non-scalar discriminant has no sensible port name, so it must route to
        // `default` rather than JSON-dumping the object as a (never-matching) port.
        let (port, _) = route(
            json!({ "field": "obj" }),
            vec![Item::new(json!({ "obj": { "k": "v" } }))],
        )
        .await;
        assert_eq!(port, "default");
    }

    #[tokio::test]
    async fn array_discriminant_routes_default() {
        // Same rule for arrays: no JSON dump as a port name.
        let (port, _) = route(
            json!({ "field": "arr" }),
            vec![Item::new(json!({ "arr": [1, 2, 3] }))],
        )
        .await;
        assert_eq!(port, "default");
    }

    #[tokio::test]
    async fn explicit_null_discriminant_routes_default() {
        // An explicit JSON `null` discriminant routes to `default`.
        let (port, _) = route(
            json!({ "field": "n" }),
            vec![Item::new(json!({ "n": null }))],
        )
        .await;
        assert_eq!(port, "default");
    }

    #[tokio::test]
    async fn missing_field_key_routes_default() {
        let (port, _) = route(
            json!({ "field": "absent" }),
            vec![Item::new(json!({ "type": "a" }))],
        )
        .await;
        assert_eq!(port, "default");
    }

    #[tokio::test]
    async fn no_discriminant_config_routes_default() {
        let (port, _) = route(Value::Null, vec![Item::new(json!({ "type": "a" }))]).await;
        assert_eq!(port, "default");
    }

    #[tokio::test]
    async fn empty_input_routes_default_and_passes_no_items() {
        let (port, items) = route(json!({ "field": "type" }), vec![]).await;
        assert_eq!(port, "default");
        assert!(items.is_empty());
    }

    #[tokio::test]
    async fn all_input_items_are_forwarded_on_the_chosen_port() {
        let input = vec![
            Item::new(json!({ "type": "a", "i": 1 })),
            Item::new(json!({ "i": 2 })),
        ];
        let (port, items) = route(json!({ "field": "type" }), input.clone()).await;
        assert_eq!(port, "a", "the first item selects the port");
        assert_eq!(items, input, "every input item is routed through");
    }

    #[tokio::test]
    async fn switch_routes_only_the_matching_case() {
        // trigger -> switch(expression = item.type) branches to pass_a (port "a")
        // and pass_b (port "b"), both passthroughs. Input type "a" must run only
        // the "a" branch.
        let mut switch = node("sw", NodeKind::Switch);
        switch.config = json!({ "expression": "=item.type" });
        let graph = WorkflowGraph {
            nodes: vec![
                node("t", NodeKind::Trigger),
                switch,
                node("pass_a", NodeKind::OutputParser),
                node("pass_b", NodeKind::OutputParser),
            ],
            edges: vec![
                Edge {
                    from_node: "t".to_string(),
                    from_port: "main".to_string(),
                    to_node: "sw".to_string(),
                    to_port: "main".to_string(),
                },
                Edge {
                    from_node: "sw".to_string(),
                    from_port: "a".to_string(),
                    to_node: "pass_a".to_string(),
                    to_port: "main".to_string(),
                },
                Edge {
                    from_node: "sw".to_string(),
                    from_port: "b".to_string(),
                    to_node: "pass_b".to_string(),
                    to_port: "main".to_string(),
                },
            ],
            ..Default::default()
        };
        let compiled = compile(&graph).expect("compile");
        let caps = mock_capabilities();

        let outcome = run(&compiled, json!({ "type": "a" }), &caps)
            .await
            .expect("run");
        assert!(
            !outcome.output["nodes"]["pass_a"]["items"].is_null(),
            "the \"a\" branch should have run"
        );
        assert!(
            outcome.output["nodes"]["pass_b"].is_null(),
            "the \"b\" branch should not have run"
        );
    }
}
