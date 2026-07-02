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
        let scope = serde_json::json!({ "item": item, "run": ctx.run });
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
