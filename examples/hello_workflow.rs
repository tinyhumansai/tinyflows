//! Minimal tinyflows demo: build a workflow, compile it, run it against mocks.
//! Run with:  cargo run --example hello_workflow --features mock

#[cfg(feature = "mock")]
#[tokio::main(flavor = "current_thread")]
async fn main() {
    use serde_json::{Value, json};
    use tinyflows::caps::mock::mock_capabilities;
    use tinyflows::compiler::compile;
    use tinyflows::engine::run;
    use tinyflows::model::{Edge, Node, NodeKind, WorkflowGraph};

    let graph = WorkflowGraph {
        nodes: vec![
            Node {
                id: "t".into(),
                kind: NodeKind::Trigger,
                name: "start".into(),
                config: Value::Null,
                ports: vec![],
                position: None,
            },
            Node {
                id: "greet".into(),
                kind: NodeKind::Transform,
                name: "greet".into(),
                config: json!({ "set": { "greeting": "=item.name" } }),
                ports: vec![],
                position: None,
            },
        ],
        edges: vec![Edge {
            from_node: "t".into(),
            from_port: "main".into(),
            to_node: "greet".into(),
            to_port: "main".into(),
        }],
        ..Default::default()
    };

    let compiled = compile(&graph).expect("compile");
    let outcome = run(&compiled, json!({ "name": "Ada" }), &mock_capabilities())
        .await
        .expect("run");
    println!("{}", serde_json::to_string_pretty(&outcome.output).unwrap());
}

#[cfg(not(feature = "mock"))]
fn main() {
    eprintln!("Run with the `mock` feature: cargo run --example hello_workflow --features mock");
}
