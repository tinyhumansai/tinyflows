//! The `merge` node: an item-concatenating fan-in.

use async_trait::async_trait;

use crate::error::Result;
use crate::nodes::{NodeContext, NodeExecutor, NodeOutput};

/// Fan-in that combines the items arriving from multiple predecessors.
///
/// The engine already concatenates every predecessor's items into `ctx.input`,
/// so at runtime `merge` is a passthrough of that combined stream.
#[derive(Debug, Default, Clone)]
pub struct MergeNode;

#[async_trait]
impl NodeExecutor for MergeNode {
    async fn execute(&self, ctx: NodeContext<'_>) -> Result<NodeOutput> {
        // Fan-in: the engine concatenates all predecessor items into `ctx.input`, so
        // merge emits them combined. (A true multi-branch barrier via waiting edges
        // lands with parallel fan-out support — see docs/04.)
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
            caps: &caps,
        };

        let output = MergeNode.execute(ctx).await.expect("execute");

        assert_eq!(output.items.len(), 2);
        assert_eq!(output.items, input);
    }
}
