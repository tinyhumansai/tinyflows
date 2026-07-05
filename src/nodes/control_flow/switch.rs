//! The `switch` node: a multi-way branch.

use async_trait::async_trait;

use crate::error::Result;
use crate::nodes::{NodeContext, NodeExecutor, NodeOutput};

/// Multi-way branch keyed by a computed case value.
///
/// The case key comes from config: an `expression` (an `=`-expression evaluated
/// against `{ item, run }`) takes precedence, otherwise a `field` names a key on
/// the first input item. The resulting value selects the output port to emit on,
/// routing to the matching case; a `null` result routes to the `default` port.
#[derive(Debug, Default, Clone)]
pub struct SwitchNode;

#[async_trait]
impl NodeExecutor for SwitchNode {
    async fn execute(&self, ctx: NodeContext<'_>) -> Result<NodeOutput> {
        let item = ctx
            .input
            .first()
            .map(|i| i.json.clone())
            .unwrap_or(serde_json::Value::Null);
        let items: Vec<serde_json::Value> = ctx.input.iter().map(|i| i.json.clone()).collect();
        let scope = serde_json::json!({ "item": item, "items": items, "run": ctx.run });
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
        let port = match value {
            serde_json::Value::String(s) => s,
            serde_json::Value::Null => "default".to_string(),
            other => other.to_string(),
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
