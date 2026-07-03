//! jq expressions: a transform node computes derived fields with jaq (`=`-programs)
//! over the input item.
//! Run:  cargo run --example jq_expressions --features mock
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

    // trigger -> compute(transform): each `set` value is a jaq program run over
    // { item, run }.
    let graph = WorkflowGraph {
        nodes: vec![
            node("trigger", NodeKind::Trigger, Value::Null),
            node(
                "compute",
                NodeKind::Transform,
                json!({
                    "set": {
                        "total": "=.item.prices | add",
                        "count": "=.item.prices | length",
                        "first": "=.item.prices[0]"
                    }
                }),
            ),
        ],
        edges: vec![edge("trigger", "main", "compute")],
        ..Default::default()
    };

    let compiled = compile(&graph).expect("compile");
    let outcome = run(
        &compiled,
        json!({ "prices": [10, 20, 30] }),
        &mock_capabilities(),
    )
    .await
    .expect("run");

    let out = &outcome.output["nodes"]["compute"]["items"][0]["json"];
    println!("input prices: [10, 20, 30]");
    println!("total = {}", out["total"]);
    println!("count = {}", out["count"]);
    println!("first = {}", out["first"]);
    assert_eq!(out["total"], json!(60));
    assert_eq!(out["count"], json!(3));
    assert_eq!(out["first"], json!(10));
    println!("=> jaq computed total=60, count=3, first=10");
}

#[cfg(not(feature = "mock"))]
fn main() {
    eprintln!("Run with the `mock` feature: cargo run --example jq_expressions --features mock");
}
