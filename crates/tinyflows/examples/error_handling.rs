//! Error handling: a failing tool_call retries, then routes its error item out the
//! `error` port to a recovery node instead of ending the run.
//! Run:  cargo run --example error_handling --features mock
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

    // `flaky` has NO slug, so the tool_call deterministically errors. With
    // on_error=route it retries twice, then emits the error on the `error` port,
    // which an edge carries into `recover`.
    let graph = WorkflowGraph {
        nodes: vec![
            node("trigger", NodeKind::Trigger, Value::Null),
            node(
                "flaky",
                NodeKind::ToolCall,
                json!({
                    "on_error": "route",
                    "retry": { "max_attempts": 2, "backoff_ms": 1 }
                }),
            ),
            node("recover", NodeKind::OutputParser, Value::Null),
        ],
        edges: vec![
            edge("trigger", "main", "flaky"),
            edge("flaky", "error", "recover"),
        ],
        ..Default::default()
    };

    let compiled = compile(&graph).expect("compile");
    let outcome = run(&compiled, json!({}), &mock_capabilities())
        .await
        .expect("run");

    let err = &outcome.output["nodes"]["recover"]["items"][0]["json"]["error"];
    println!("run completed despite the tool failure (retry + error-port routing)");
    println!(
        "error item that reached `recover`: {}",
        serde_json::to_string(err).unwrap()
    );
    assert_eq!(err["node"], json!("flaky"));
    println!(
        "=> failure from node `{}` was recovered, not fatal",
        err["node"]
    );
}

#[cfg(not(feature = "mock"))]
fn main() {
    eprintln!("Run with the `mock` feature: cargo run --example error_handling --features mock");
}
