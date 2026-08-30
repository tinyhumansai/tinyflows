#![cfg(feature = "mock")]
//! The retries-exhausted warning has to carry the cause.
//!
//! `[outcome] node failed after retries` is the only line emitted when a node
//! gives up, and it used to carry the node id and nothing else — so a report of
//! it (openhuman#5626) could not be root-caused at all: the log said a node
//! failed but not why, and not how many attempts it took.
//!
//! What is pinned here is the *presence* of the diagnostic fields, not their
//! exact rendering. An assertion that the `error` field merely renders
//! non-empty would still pass if the field were dropped again, because a
//! missing field and an empty one are indistinguishable once formatted — so
//! these look the field up by name and fail if it is absent.
//!
//! The failing node is a `tool_call` with no `slug`, the same deterministic
//! failure `tests/error_recovery_e2e.rs` uses: its executor returns a
//! `Capability` error on every attempt.
//!
//! Gated behind the `mock` cargo feature.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tracing::field::{Field, Visit};
use tracing::span;
use tracing::{Event, Metadata, Subscriber};

use tinyflows::caps::mock::mock_capabilities;
use tinyflows::compiler::compile;
use tinyflows::engine::run;
use tinyflows::model::{Edge, Node, NodeKind, TriggerKind, WorkflowGraph};

/// One captured `tracing` event: its message plus every other field, keyed by
/// name so a test can distinguish "absent" from "empty".
#[derive(Clone, Debug)]
struct CapturedEvent {
    message: String,
    fields: BTreeMap<String, String>,
}

#[derive(Default)]
struct FieldVisitor {
    message: String,
    fields: BTreeMap<String, String>,
}

impl Visit for FieldVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        let rendered = format!("{value:?}");
        if field.name() == "message" {
            self.message = rendered;
        } else {
            self.fields.insert(field.name().to_string(), rendered);
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.fields
                .insert(field.name().to_string(), value.to_string());
        }
    }
}

/// A minimal global subscriber that keeps every event.
///
/// Hand-rolled rather than pulling in `tracing-subscriber`: this crate takes
/// `tracing` with `default-features = false` and carries no subscriber
/// dependency, and one visitor is cheaper than a new dev-dependency.
/// Registered globally rather than with `with_default` because the engine runs
/// on tokio worker threads and a thread-local dispatcher would miss them.
struct CapturingSubscriber {
    events: Arc<Mutex<Vec<CapturedEvent>>>,
}

impl Subscriber for CapturingSubscriber {
    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _attrs: &span::Attributes<'_>) -> span::Id {
        span::Id::from_u64(1)
    }

    fn record(&self, _span: &span::Id, _values: &span::Record<'_>) {}

    fn record_follows_from(&self, _span: &span::Id, _follows: &span::Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);
        self.events
            .lock()
            .expect("captured events mutex poisoned")
            .push(CapturedEvent {
                message: visitor.message,
                fields: visitor.fields,
            });
    }

    fn enter(&self, _span: &span::Id) {}

    fn exit(&self, _span: &span::Id) {}
}

fn node(id: &str, kind: NodeKind, config: Value) -> Node {
    Node {
        id: id.to_string(),
        kind,
        type_version: 1,
        name: id.to_string(),
        config,
        ports: vec![],
        position: None,
    }
}

fn trigger(id: &str) -> Node {
    node(
        id,
        NodeKind::Trigger,
        json!({ "kind": TriggerKind::Manual }),
    )
}

fn edge(from_node: &str, from_port: &str, to_node: &str) -> Edge {
    Edge {
        from_node: from_node.to_string(),
        from_port: from_port.to_string(),
        to_node: to_node.to_string(),
        to_port: "main".to_string(),
    }
}

#[tokio::test]
async fn retries_exhausted_warning_reports_the_cause_and_the_attempt_count() {
    let events = Arc::new(Mutex::new(Vec::new()));
    tracing::subscriber::set_global_default(CapturingSubscriber {
        events: Arc::clone(&events),
    })
    .expect("this test binary registers the only global subscriber");

    // trigger -> failing tool_call, three attempts, default `stop` policy.
    let graph = WorkflowGraph {
        nodes: vec![
            trigger("t"),
            node(
                "summarize",
                NodeKind::ToolCall,
                json!({ "retry": { "max_attempts": 3 } }),
            ),
        ],
        edges: vec![edge("t", "main", "summarize")],
        ..Default::default()
    };
    let compiled = compile(&graph).expect("graph should compile");
    let _ = run(&compiled, json!({}), &mock_capabilities()).await;

    let captured = events
        .lock()
        .expect("captured events mutex poisoned")
        .clone();
    let warning = captured
        .iter()
        .find(|event| event.message == "node failed after retries")
        .unwrap_or_else(|| panic!("expected a retries-exhausted warning, captured: {captured:#?}"));

    assert_eq!(
        warning.fields.get("node").map(String::as_str),
        Some("summarize"),
        "the warning must name the node; fields were {:?}",
        warning.fields
    );

    // The point of the change: the cause has to be on the line. Looked up by
    // name so a dropped field fails here rather than rendering as empty.
    let error = warning.fields.get("error").unwrap_or_else(|| {
        panic!(
            "the retries-exhausted warning carries no `error` field, so nothing \
             in the log can say why the node failed; fields were {:?}",
            warning.fields
        )
    });
    assert!(
        error.contains("Capability"),
        "the `error` field must render the executor's error, got {error:?}"
    );

    // "after retries" with no number is the second thing you want and don't
    // have: three configured attempts, three reported.
    assert_eq!(
        warning.fields.get("attempts").map(String::as_str),
        Some("3"),
        "the warning must report how many attempts ran; fields were {:?}",
        warning.fields
    );

    assert_eq!(
        warning.fields.get("on_error").map(String::as_str),
        Some("stop"),
        "the warning must report the policy that decided what happened next; \
         fields were {:?}",
        warning.fields
    );
}
