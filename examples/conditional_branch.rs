//! Conditional branch: an IF node routes to exactly one of two tool_call branches.
//! Run:  cargo run --example conditional_branch --features mock
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

    // trigger -> condition("check") -> true:welcome / false:upsell
    let graph = WorkflowGraph {
        nodes: vec![
            node("trigger", NodeKind::Trigger, Value::Null),
            node(
                "check",
                NodeKind::Condition,
                json!({ "field": "is_member" }),
            ),
            node(
                "welcome",
                NodeKind::ToolCall,
                json!({ "slug": "slack.welcome" }),
            ),
            node(
                "upsell",
                NodeKind::ToolCall,
                json!({ "slug": "crm.upsell" }),
            ),
        ],
        edges: vec![
            edge("trigger", "main", "check"),
            edge("check", "true", "welcome"),
            edge("check", "false", "upsell"),
        ],
        ..Default::default()
    };

    let compiled = compile(&graph).expect("compile");
    let outcome = run(
        &compiled,
        json!({ "is_member": true }),
        &mock_capabilities(),
    )
    .await
    .expect("run");

    let welcome_ran = !outcome.output["nodes"]["welcome"].is_null();
    let upsell_ran = !outcome.output["nodes"]["upsell"].is_null();
    println!("input: {{ \"is_member\": true }}");
    println!("welcome branch ran: {welcome_ran}");
    println!("upsell branch ran:  {upsell_ran}");
    assert!(
        welcome_ran && !upsell_ran,
        "only the welcome branch should run"
    );
    let tool = &outcome.output["nodes"]["welcome"]["items"][0]["json"]["tool"];
    println!("=> took the TRUE branch; welcome called tool {tool}");
}

#[cfg(not(feature = "mock"))]
fn main() {
    eprintln!(
        "Run with the `mock` feature: cargo run --example conditional_branch --features mock"
    );
}
