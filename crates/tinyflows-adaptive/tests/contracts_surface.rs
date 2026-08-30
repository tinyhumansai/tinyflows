//! What crosses a process boundary, asserted rather than assumed.
//!
//! Every type here is part of a contract some other process — a device runner,
//! a TypeScript relay, a third ledger backend — has to produce or read. A
//! derive quietly dropped from one of them is a runtime failure in a different
//! repository, so the requirement is checked at compile time here.

use serde::Serialize;
use serde::de::DeserializeOwned;

const fn wire<T: Serialize + DeserializeOwned>() {}

#[test]
fn every_wire_type_still_serializes_both_ways() {
    // The execute contract: what a runner receives and returns.
    wire::<tinyflows_adaptive::execute::RunRequest>();
    wire::<tinyflows_adaptive::execute::RunReport>();
    wire::<tinyflows_adaptive::execute::StepRecord>();
    wire::<tinyflows_adaptive::execute::StepOutcome>();

    // Inside those: engine types the runner must round-trip untouched.
    wire::<tinyflows::model::WorkflowGraph>();
    wire::<tinyflows::model::Node>();
    wire::<tinyflows::model::Edge>();
    wire::<tinyflows::model::WorkflowInput>();
    wire::<tinyflows::expr::NullResolution>();

    // Derived on the loop's side from the steps, but stored and shipped by
    // hosts that keep a run record.
    wire::<tinyflows::diagnostics::Diagnosis>();
    wire::<tinyflows::diagnostics::NullBinding>();
    wire::<tinyflows::diagnostics::HiddenError>();
    wire::<tinyflows::diagnostics::NeverRan>();

    // The loop's own persisted state: anything a hosted service stores.
    wire::<tinyflows_adaptive::contracts::Goal>();
    wire::<tinyflows_adaptive::contracts::Approach>();
    wire::<tinyflows_adaptive::contracts::Verdict>();
    wire::<tinyflows_adaptive::contracts::Blocker>();
    wire::<tinyflows_adaptive::contracts::Budget>();
    wire::<tinyflows_adaptive::ledger::LedgerRow>();
    wire::<tinyflows_adaptive::ledger::Lesson>();
    wire::<tinyflows_adaptive::ledger::LessonKind>();
    wire::<tinyflows_adaptive::ledger::Score>();

    // What a device reports about itself, and what a repair proposes.
    wire::<tinyflows_adaptive::host::HostFacts>();
    wire::<tinyflows::graph_ops::GraphOp>();
    wire::<tinyflows::store::types::WorkflowRecord>();
}

#[test]
fn the_envelope_is_camel_case_and_the_graph_inside_it_is_not() {
    // Worth pinning because it will bite whoever writes the other side. The
    // wire types this crate added use camelCase; the engine's own model types
    // predate them and use serde's default. One payload, two conventions.
    let request = tinyflows_adaptive::execute::RunRequest {
        attempt_id: "ep-1/3".into(),
        graph: tinyflows::model::WorkflowGraph {
            schema_version: 1,
            id: Some("g".into()),
            name: "g".into(),
            inputs: Vec::new(),
            agents: Vec::new(),
            nodes: vec![tinyflows::model::Node {
                id: "start".into(),
                kind: tinyflows::model::NodeKind::Trigger,
                type_version: 1,
                name: "start".into(),
                config: serde_json::json!({"trigger_kind": "manual"}),
                ports: Vec::new(),
                position: None,
            }],
            edges: vec![tinyflows::model::Edge {
                from_node: "start".into(),
                from_port: "main".into(),
                to_node: "start".into(),
                to_port: "main".into(),
            }],
        },
        inputs: serde_json::Map::new(),
    };
    let text = serde_json::to_string(&request).expect("serializes");

    assert!(
        text.contains("\"attemptId\""),
        "envelope is camelCase: {text}"
    );
    assert!(
        text.contains("\"schema_version\""),
        "the graph keeps the engine's snake_case: {text}"
    );
    assert!(text.contains("\"from_node\""), "{text}");
    assert!(text.contains("\"type_version\""), "{text}");

    println!("REQUEST {text}");
    println!(
        "REPORT {}",
        serde_json::to_string(&tinyflows_adaptive::execute::RunReport {
            attempt_id: "ep-1/3".into(),
            steps: vec![tinyflows_adaptive::execute::StepRecord {
                node_id: "start".into(),
                status: tinyflows_adaptive::execute::StepOutcome::Success,
                output: serde_json::json!({"ok": true}),
                duration_ms: 12,
                null_bindings: Vec::new(),
                transcript: Vec::new(),
            }],
            pending_approvals: vec!["publish".into()],
            cancelled: false,
            changed: "1 file changed".into(),
            failed: None,
            cost_usd: 0.42,
        })
        .expect("serializes")
    );
}

const fn shareable<T: Send + Sync>() {}

#[test]
fn a_loop_can_be_shared_across_tasks_and_replicas() {
    // The operational half of statelessness. `Loop` holds only borrows of
    // `Send + Sync` adapters and no state of its own, so one instance serves
    // many concurrent episodes and any replica can serve any request. If this
    // stops compiling, something acquired state that has to be owned — and the
    // microservice story goes with it.
    shareable::<tinyflows_adaptive::driver::Loop<'_>>();

    // The adapters a host injects, for the same reason.
    shareable::<&dyn tinyflows_adaptive::ledger::Ledger>();
    shareable::<&dyn tinyflows_adaptive::execute::Runner>();
    shareable::<&dyn tinyflows_adaptive::execute::Relay>();
    shareable::<&dyn tinyflows_adaptive::execute::Workspace>();
    shareable::<&dyn tinyflows_adaptive::driver::Clock>();

    // And the values that cross between them.
    shareable::<tinyflows_adaptive::ledger::Episode>();
    shareable::<tinyflows_adaptive::execute::Ran>();
    shareable::<tinyflows_adaptive::closing::Closed>();
}
