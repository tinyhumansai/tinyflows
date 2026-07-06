//! The `http_request` node: an outbound HTTP request.

use async_trait::async_trait;
use serde_json::Value;

use crate::data::Item;
use crate::error::Result;
use crate::nodes::integration::envelope;
use crate::nodes::{ExecutionMode, NodeContext, NodeExecutor, NodeOutput, execution_mode};

/// Performs an outbound HTTP request via [`crate::caps::HttpClient`].
///
/// **Execution** (`config.execution`, default `per_item`): `per_item` maps over
/// the input, issuing one request per item with the descriptor re-resolved
/// against that item; `once` issues a single request against the first item.
/// Output is wrapped in the stable `{ json, text, raw }`
/// [envelope](crate::nodes::integration::envelope), matching the other
/// capability nodes.
#[derive(Debug, Default, Clone)]
pub struct HttpRequestNode;

/// Issues the request described by an already-resolved `cfg`.
async fn request(ctx: &NodeContext<'_>, cfg: &Value) -> Result<Value> {
    // The node's config is the request descriptor; the host's HttpClient interprets it.
    let conn = cfg.get("connection_ref").and_then(Value::as_str);
    ctx.caps.http.request(cfg.clone(), conn).await
}

#[async_trait]
impl NodeExecutor for HttpRequestNode {
    async fn execute(&self, ctx: NodeContext<'_>) -> Result<NodeOutput> {
        let per_item = execution_mode(&ctx.node.config, ExecutionMode::PerItem)
            == ExecutionMode::PerItem
            && !ctx.input.is_empty();

        if per_item {
            let mut items = Vec::with_capacity(ctx.input.len());
            let mut diagnostics = Vec::new();
            for (index, input_item) in ctx.input.iter().enumerate() {
                let (cfg, diags) =
                    crate::nodes::resolve_config_traced_for_item(&ctx, input_item.json.clone());
                let response = request(&ctx, &cfg).await?;
                items.push(Item::new(envelope::wrap(response)).paired_with(index));
                diagnostics.extend(diags);
            }
            Ok(NodeOutput::main(items).with_diagnostics(diagnostics))
        } else {
            let (cfg, diagnostics) = crate::nodes::resolve_config_traced(&ctx);
            let response = request(&ctx, &cfg).await?;
            Ok(NodeOutput::main(vec![Item::new(envelope::wrap(response))])
                .with_diagnostics(diagnostics))
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::caps::mock::mock_capabilities;
    use crate::compiler::compile;
    use crate::engine::run;
    use crate::model::{Edge, Node, NodeKind, WorkflowGraph};
    use serde_json::{Value, json};

    fn wf(kind: NodeKind, config: Value) -> WorkflowGraph {
        WorkflowGraph {
            nodes: vec![
                Node {
                    id: "t".into(),
                    kind: NodeKind::Trigger,
                    type_version: 1,
                    name: "t".into(),
                    config: Value::Null,
                    ports: vec![],
                    position: None,
                },
                Node {
                    id: "n".into(),
                    kind,
                    type_version: 1,
                    name: "n".into(),
                    config,
                    ports: vec![],
                    position: None,
                },
            ],
            edges: vec![Edge {
                from_node: "t".into(),
                from_port: "main".into(),
                to_node: "n".into(),
                to_port: "main".into(),
            }],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn http_request_returns_mock_response() {
        let graph = wf(
            NodeKind::HttpRequest,
            json!({ "method": "GET", "url": "https://example.com" }),
        );
        let compiled = compile(&graph).expect("compile");
        let out = run(&compiled, json!({ "seed": 1 }), &mock_capabilities())
            .await
            .expect("run");
        // Output is enveloped: the response is under `json.*`.
        assert_eq!(
            out.output["nodes"]["n"]["items"][0]["json"]["json"]["status"],
            200
        );
    }

    use super::HttpRequestNode;
    use crate::data::Item;
    use crate::nodes::{NodeContext, NodeExecutor};

    #[tokio::test]
    async fn echoes_method_url_and_threads_connection() {
        let node = Node {
            id: "n".into(),
            kind: NodeKind::HttpRequest,
            type_version: 1,
            name: "n".into(),
            config: json!({ "method": "POST", "url": "https://api.test/x", "connection_ref": "http:acct_2" }),
            ports: vec![],
            position: None,
        };
        let input = vec![Item::new(json!({ "seed": 1 }))];
        let caps = mock_capabilities();
        let run_meta = Value::Null;
        let ctx = NodeContext {
            node: &node,
            input: &input,
            run: &run_meta,
            nodes: &Value::Null,
            caps: &caps,
        };
        let out = HttpRequestNode.execute(ctx).await.expect("execute");
        assert_eq!(out.items.len(), 1);
        // The mock HTTP client echoes the request descriptor and the conn ref,
        // under the envelope's `json`.
        assert_eq!(out.items[0].json["json"]["status"], 200);
        assert_eq!(out.items[0].json["json"]["request"]["method"], "POST");
        assert_eq!(
            out.items[0].json["json"]["request"]["url"],
            "https://api.test/x"
        );
        assert_eq!(out.items[0].json["json"]["connection"], "http:acct_2");
    }

    #[tokio::test]
    async fn resolves_expressions_in_config_against_input() {
        // `url` and `body.q` are `=`-expressions bound to the input item; the mock
        // HTTP client echoes the resolved request descriptor.
        let node = Node {
            id: "n".into(),
            kind: NodeKind::HttpRequest,
            type_version: 1,
            name: "n".into(),
            config: json!({ "method": "POST", "url": "=item.url", "body": { "q": "=item.q" } }),
            ports: vec![],
            position: None,
        };
        let input = vec![Item::new(json!({ "url": "https://a", "q": "hi" }))];
        let caps = mock_capabilities();
        let run_meta = Value::Null;
        let ctx = NodeContext {
            node: &node,
            input: &input,
            run: &run_meta,
            nodes: &Value::Null,
            caps: &caps,
        };
        let out = HttpRequestNode.execute(ctx).await.expect("execute");
        assert_eq!(out.items[0].json["json"]["request"]["url"], "https://a");
        assert_eq!(out.items[0].json["json"]["request"]["body"]["q"], "hi");
        assert_eq!(out.items[0].paired_item, Some(0));
    }

    #[tokio::test]
    async fn missing_connection_ref_is_null() {
        let node = Node {
            id: "n".into(),
            kind: NodeKind::HttpRequest,
            type_version: 1,
            name: "n".into(),
            config: json!({ "method": "GET", "url": "u" }),
            ports: vec![],
            position: None,
        };
        let input = vec![];
        let caps = mock_capabilities();
        let run_meta = Value::Null;
        let ctx = NodeContext {
            node: &node,
            input: &input,
            run: &run_meta,
            nodes: &Value::Null,
            caps: &caps,
        };
        let out = HttpRequestNode.execute(ctx).await.expect("execute");
        assert_eq!(out.items[0].json["json"]["connection"], Value::Null);
    }
}
