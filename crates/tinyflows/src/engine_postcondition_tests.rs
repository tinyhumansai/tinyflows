//! The engine enforcing a node's declared `postcondition`: an output that
//! fails it is a failed attempt, retried and then handed to `on_error`.

use super::*;
use crate::caps::AgentRunner;
use crate::caps::mock::{mock_capabilities, mock_capabilities_with_agent};
use crate::compiler::compile;
use crate::model::{Edge, Node, WorkflowGraph};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

/// An agent harness that answers each call with the next scripted reply,
/// repeating the last one once the script runs out.
struct Scripted {
    replies: Mutex<Vec<Value>>,
    calls: AtomicUsize,
}

impl Scripted {
    fn new(replies: Vec<Value>) -> Self {
        Self {
            replies: Mutex::new(replies),
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl AgentRunner for Scripted {
    async fn run_agent(
        &self,
        _agent_ref: &str,
        _request: Value,
        _conn: Option<&str>,
    ) -> crate::error::Result<Value> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut replies = self.replies.lock().unwrap();
        Ok(if replies.len() > 1 {
            replies.remove(0)
        } else {
            replies[0].clone()
        })
    }
}

/// `trigger -> x`, where `x` is a node of `kind` with `config`.
fn graph(kind: NodeKind, config: Value) -> WorkflowGraph {
    let node = |id: &str, kind: NodeKind, config: Value| Node {
        id: id.to_string(),
        kind,
        type_version: 1,
        name: id.to_string(),
        config,
        ports: Vec::new(),
        position: None,
    };
    WorkflowGraph {
        nodes: vec![
            node("t", NodeKind::Trigger, Value::Null),
            node("x", kind, config),
        ],
        edges: vec![Edge {
            from_node: "t".to_string(),
            from_port: "main".to_string(),
            to_node: "x".to_string(),
            to_port: "main".to_string(),
        }],
        ..Default::default()
    }
}

fn agent(config: Value) -> WorkflowGraph {
    let mut config = config;
    config["agent_ref"] = json!("writer");
    config["prompt"] = json!("write it");
    graph(NodeKind::Agent, config)
}

#[tokio::test]
async fn an_output_that_clears_the_postcondition_advances() {
    let compiled = compile(&graph(
        NodeKind::ToolCall,
        json!({ "slug": "search", "postcondition": { "require": "field_present", "field": "json.tool" } }),
    ))
    .expect("compile");
    let outcome = run(&compiled, json!({}), &mock_capabilities())
        .await
        .expect("run");
    assert_eq!(
        outcome.output["nodes"]["x"]["items"][0]["json"]["json"]["tool"],
        json!("search")
    );
}

/// The default `on_error` is `stop`: an insufficient output never reaches
/// anything downstream, and the error names the node and the gap.
#[tokio::test]
async fn an_output_that_fails_the_postcondition_stops_the_run() {
    let runner = Scripted::new(vec![json!("   ")]);
    let compiled = compile(&agent(
        json!({ "postcondition": { "require": "non_empty" } }),
    ))
    .expect("compile");
    let err = run(&compiled, json!({}), &mock_capabilities_with_agent(runner))
        .await
        .expect_err("an empty reply must not advance");
    let message = err.to_string();
    assert!(message.contains("'x'"), "{message}");
    assert!(message.contains("postcondition"), "{message}");
    assert!(message.contains("empty"), "{message}");
}

/// A failed postcondition is a failed attempt, so `retry` re-runs the node and
/// a later sufficient output is what flows on.
#[tokio::test]
async fn a_failed_postcondition_is_retried_like_any_failed_attempt() {
    let runner = std::sync::Arc::new(Scripted::new(vec![json!(""), json!("the answer")]));
    let caps = crate::caps::Capabilities {
        agent: Some(runner.clone()),
        ..mock_capabilities()
    };
    let compiled = compile(&agent(json!({
        "retry": { "max_attempts": 3 },
        "postcondition": { "require": "non_empty" }
    })))
    .expect("compile");
    let outcome = run(&compiled, json!({}), &caps).await.expect("run");
    assert_eq!(runner.calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        outcome.output["nodes"]["x"]["items"][0]["json"]["text"],
        json!("the answer")
    );
}

/// Once attempts run out, `on_error` decides — `continue` emits the error
/// item carrying the gap instead of the insufficient output.
#[tokio::test]
async fn an_exhausted_postcondition_honours_on_error() {
    let runner = Scripted::new(vec![json!({ "items": [] })]);
    let compiled = compile(&agent(json!({
        "on_error": "continue",
        "postcondition": { "require": "non_empty_list", "field": "json.items" }
    })))
    .expect("compile");
    let outcome = run(&compiled, json!({}), &mock_capabilities_with_agent(runner))
        .await
        .expect("run");
    let error = &outcome.output["nodes"]["x"]["items"][0]["json"]["error"];
    assert_eq!(error["node"], json!("x"));
    assert!(
        error["message"]
            .as_str()
            .is_some_and(|m| m.contains("`json.items` is an empty list")),
        "{error}"
    );
}

/// A node with no declaration is untouched: the gate is opt-in.
#[tokio::test]
async fn a_node_without_a_postcondition_is_not_checked() {
    let runner = Scripted::new(vec![json!("")]);
    let compiled = compile(&agent(json!({}))).expect("compile");
    let outcome = run(&compiled, json!({}), &mock_capabilities_with_agent(runner))
        .await
        .expect("run");
    assert_eq!(
        outcome.output["nodes"]["x"]["items"][0]["json"]["text"],
        json!("")
    );
}
