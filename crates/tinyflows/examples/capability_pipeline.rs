//! Capability pipeline: a linear chain that touches every host capability
//! (http -> code -> agent/llm -> tool) through the mock implementations.
//! Run:  cargo run --example capability_pipeline --features mock
#[cfg(feature = "mock")]
#[tokio::main(flavor = "current_thread")]
async fn main() {
    use serde_json::{Value, json};
    use tinyflows::caps::mock::mock_capabilities;
    use tinyflows::compiler::compile;
    use tinyflows::engine::run;
    use tinyflows::model::{Edge, Node, NodeKind, WorkflowGraph};

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
    fn edge(from: &str, port: &str, to: &str) -> Edge {
        Edge {
            from_node: from.into(),
            from_port: port.into(),
            to_node: to.into(),
            to_port: "main".into(),
        }
    }

    // trigger(webhook) -> fetch(http) -> shape(code) -> summarize(agent) -> post(tool)
    let graph = WorkflowGraph {
        nodes: vec![
            node("trigger", NodeKind::Trigger, json!({ "kind": "webhook" })),
            node(
                "fetch",
                NodeKind::HttpRequest,
                json!({ "method": "GET", "url": "https://api" }),
            ),
            node(
                "shape",
                NodeKind::Code,
                json!({ "language": "javascript", "source": "return items;" }),
            ),
            node(
                "summarize",
                NodeKind::Agent,
                json!({ "prompt": "summarize" }),
            ),
            node(
                "post",
                NodeKind::ToolCall,
                json!({ "slug": "slack.post", "args": { "channel": "#ops" } }),
            ),
        ],
        edges: vec![
            edge("trigger", "main", "fetch"),
            edge("fetch", "main", "shape"),
            edge("shape", "main", "summarize"),
            edge("summarize", "main", "post"),
        ],
        ..Default::default()
    };

    let compiled = compile(&graph).expect("compile");
    let outcome = run(&compiled, json!({ "run": 1 }), &mock_capabilities())
        .await
        .expect("run");

    let post = &outcome.output["nodes"]["post"]["items"][0]["json"];
    println!("pipeline ran: http -> code -> agent -> tool");
    println!(
        "final `post` item: {}",
        serde_json::to_string(post).unwrap()
    );
    assert_eq!(post["tool"], json!("slack.post"));
    assert_eq!(post["args"]["channel"], json!("#ops"));
    println!(
        "=> tool {} posted to channel {}",
        post["tool"], post["args"]["channel"]
    );
}

#[cfg(not(feature = "mock"))]
fn main() {
    eprintln!(
        "Run with the `mock` feature: cargo run --example capability_pipeline --features mock"
    );
}
