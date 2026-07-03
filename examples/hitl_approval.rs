//! Human-in-the-loop approval: a gated node pauses the run until approved, then a
//! checkpointed resume continues from the interrupt without re-running finished nodes.
//! Run:  cargo run --example hitl_approval --features mock
#[cfg(feature = "mock")]
#[tokio::main(flavor = "current_thread")]
async fn main() {
    use serde_json::{Value, json};
    use tinyflows::caps::mock::mock_capabilities;
    use tinyflows::compiler::compile;
    use tinyflows::engine::run_resumable;
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

    // trigger -> approve{requires_approval} -> notify. The gate pauses the run.
    let graph = WorkflowGraph {
        nodes: vec![
            node("trigger", NodeKind::Trigger, Value::Null),
            node(
                "approve",
                NodeKind::ToolCall,
                json!({ "slug": "slack.send", "requires_approval": true }),
            ),
            node("notify", NodeKind::OutputParser, Value::Null),
        ],
        edges: vec![
            edge("trigger", "main", "approve"),
            edge("approve", "main", "notify"),
        ],
        ..Default::default()
    };

    let compiled = compile(&graph).expect("compile");
    let caps = mock_capabilities();

    // 1) Initial run: pauses at the approval gate.
    let rr = run_resumable(&compiled, json!({}), &caps)
        .await
        .expect("run_resumable");
    let paused = rr.outcome();
    println!("--- before approval ---");
    println!("pending_approvals: {:?}", paused.pending_approvals);
    println!(
        "notify ran: {}",
        !paused.output["nodes"]["notify"].is_null()
    );
    assert!(paused.pending_approvals.contains(&"approve".to_string()));
    assert!(paused.output["nodes"]["notify"].is_null());

    // 2) Approve and resume from the checkpoint.
    let done = rr.resume(vec!["approve".into()]).await.expect("resume");
    println!("--- after approving `approve` ---");
    println!("pending_approvals: {:?}", done.pending_approvals);
    println!("notify ran: {}", !done.output["nodes"]["notify"].is_null());
    assert!(done.pending_approvals.is_empty());
    assert!(!done.output["nodes"]["notify"].is_null());
    println!("=> gate held the run, then the checkpointed resume let `notify` run");
}

#[cfg(not(feature = "mock"))]
fn main() {
    eprintln!("Run with the `mock` feature: cargo run --example hitl_approval --features mock");
}
