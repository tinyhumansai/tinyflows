//! The `output_parser` node: structures/validates an agent's output.

use async_trait::async_trait;

use crate::error::Result;
use crate::nodes::{NodeContext, NodeExecutor, NodeOutput};

/// Parses / validates an upstream agent's output into a structured shape.
///
/// For now this is an identity passthrough of its input items. Structured
/// schema validation and LLM auto-fixing are later refinements.
#[derive(Debug, Default, Clone)]
pub struct OutputParserNode;

#[async_trait]
impl NodeExecutor for OutputParserNode {
    async fn execute(&self, ctx: NodeContext<'_>) -> Result<NodeOutput> {
        // A3-basic: pass the upstream items through unchanged. Structured-schema
        // validation and LLM auto-fixing are later refinements.
        Ok(NodeOutput::main(ctx.input.to_vec()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::mock::mock_capabilities;
    use crate::data::Item;
    use crate::model::{Node, NodeKind};
    use serde_json::{Value, json};

    #[tokio::test]
    async fn passes_items_through_unchanged() {
        let node = Node {
            id: "p".into(),
            kind: NodeKind::OutputParser,
            type_version: 1,
            name: "p".into(),
            config: Value::Null,
            ports: vec![],
            position: None,
        };
        let input = vec![Item::new(json!({ "a": 1 })), Item::new(json!({ "b": 2 }))];
        let caps = mock_capabilities();
        let ctx = NodeContext {
            node: &node,
            input: &input,
            run: &Value::Null,
            caps: &caps,
        };
        let out = OutputParserNode.execute(ctx).await.expect("execute");
        assert_eq!(out.items, input);
    }
}
