//! Parallel fan-out + merge barrier: two scans run concurrently, then merge waits
//! for both and concatenates their items.
//! Run:  cargo run --example parallel_and_merge --features mock
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

    // trigger -> dispatch, which fans out on `main` to scan_a AND scan_b,
    // both -> combine(merge) -> done.
    let graph = WorkflowGraph {
        nodes: vec![
            node("trigger", NodeKind::Trigger, Value::Null),
            node("dispatch", NodeKind::OutputParser, Value::Null),
            node(
                "scan_a",
                NodeKind::HttpRequest,
                json!({ "url": "https://a" }),
            ),
            node(
                "scan_b",
                NodeKind::HttpRequest,
                json!({ "url": "https://b" }),
            ),
            node("combine", NodeKind::Merge, Value::Null),
            node("done", NodeKind::OutputParser, Value::Null),
        ],
        edges: vec![
            edge("trigger", "main", "dispatch"),
            edge("dispatch", "main", "scan_a"),
            edge("dispatch", "main", "scan_b"),
            edge("scan_a", "main", "combine"),
            edge("scan_b", "main", "combine"),
            edge("combine", "main", "done"),
        ],
        ..Default::default()
    };

    let compiled = compile(&graph).expect("compile");
    let outcome = run(&compiled, json!({ "target": "x" }), &mock_capabilities())
        .await
        .expect("run");

    let a_ran = !outcome.output["nodes"]["scan_a"].is_null();
    let b_ran = !outcome.output["nodes"]["scan_b"].is_null();
    let merged = outcome.output["nodes"]["combine"]["items"]
        .as_array()
        .expect("merge should produce items");
    println!("scan_a ran: {a_ran}");
    println!("scan_b ran: {b_ran}");
    println!("combine (merge barrier) produced {} items", merged.len());
    assert!(a_ran && b_ran, "both parallel scans should run");
    assert_eq!(merged.len(), 2, "merge should concatenate both branches");
    println!("=> both scans ran in parallel; the merge barrier joined 2 items");
}

#[cfg(not(feature = "mock"))]
fn main() {
    eprintln!(
        "Run with the `mock` feature: cargo run --example parallel_and_merge --features mock"
    );
}
