//! Mixed browser and integration routing through ordinary `tool_call` nodes.

#![cfg(feature = "mock")]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value, json};
use tinyflows::browser::{
    BROWSER_PROTOCOL_VERSION, BrowserError, BrowserRelay, BrowserRequest, BrowserResponse,
    BrowserResult, ChromeToolInvoker, RoutingToolInvoker,
};
use tinyflows::caps::{ToolInvoker, mock::mock_capabilities};
use tinyflows::compiler::compile;
use tinyflows::engine::run;
use tinyflows::error::Result;
use tinyflows::model::{Edge, Node, NodeKind, WorkflowGraph};

#[derive(Default)]
struct Relay {
    requests: Mutex<Vec<BrowserRequest>>,
}

#[async_trait]
impl BrowserRelay for Relay {
    async fn execute(
        &self,
        request: BrowserRequest,
    ) -> std::result::Result<BrowserResponse, BrowserError> {
        self.requests.lock().unwrap().push(request.clone());
        Ok(BrowserResponse::Ok {
            protocol_version: BROWSER_PROTOCOL_VERSION,
            request_id: request.request_id,
            result: BrowserResult {
                data: json!({"title": "TinyFlows Shop"}),
            },
        })
    }
}

#[derive(Default)]
struct Integrations {
    calls: Mutex<Vec<(String, Value, Option<String>)>>,
}

#[async_trait]
impl ToolInvoker for Integrations {
    async fn invoke(&self, slug: &str, args: Value, conn: Option<&str>) -> Result<Value> {
        self.calls
            .lock()
            .unwrap()
            .push((slug.to_owned(), args.clone(), conn.map(str::to_owned)));
        Ok(json!({"sent": true, "slug": slug}))
    }
}

fn node(id: &str, kind: NodeKind, config: Value) -> Node {
    Node {
        id: id.into(),
        kind,
        type_version: 1,
        name: id.into(),
        config,
        ports: vec![],
        position: None,
    }
}

#[tokio::test]
async fn mixed_workflow_routes_browser_and_host_integration_without_ambiguity() {
    let graph = WorkflowGraph {
        name: "browser then email".into(),
        nodes: vec![
            node("start", NodeKind::Trigger, Value::Null),
            node(
                "title",
                NodeKind::ToolCall,
                json!({"slug":"browser", "args":{"action":"get_title"}}),
            ),
            node(
                "email",
                NodeKind::ToolCall,
                json!({
                    "slug":"gmail.send",
                    "connection_ref":"composio:gmail:work",
                    "args":{"subject":"Browser run complete"}
                }),
            ),
        ],
        edges: vec![
            Edge {
                from_node: "start".into(),
                from_port: "main".into(),
                to_node: "title".into(),
                to_port: "main".into(),
            },
            Edge {
                from_node: "title".into(),
                from_port: "main".into(),
                to_node: "email".into(),
                to_port: "main".into(),
            },
        ],
        ..Default::default()
    };

    let relay = Arc::new(Relay::default());
    let integrations = Arc::new(Integrations::default());
    let browser = Arc::new(ChromeToolInvoker::new(relay.clone(), "run-mixed", 42));
    let mut caps = mock_capabilities();
    caps.tools = Arc::new(RoutingToolInvoker::new(browser, integrations.clone()));

    let outcome = run(&compile(&graph).unwrap(), Value::Null, &caps)
        .await
        .unwrap();

    assert_eq!(relay.requests.lock().unwrap()[0].tab_id, 42);
    assert_eq!(relay.requests.lock().unwrap()[0].run_id, "run-mixed");
    assert_eq!(
        integrations.calls.lock().unwrap().as_slice(),
        &[(
            "gmail.send".into(),
            json!({"subject":"Browser run complete"}),
            Some("composio:gmail:work".into()),
        ),]
    );
    assert_eq!(
        outcome.output["nodes"]["email"]["items"][0]["json"]["json"]["sent"],
        true
    );
}
