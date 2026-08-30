//! End-to-end tests of the public state-graph API under default crate features.
//!
//! Most workflow integration tests use the optional `mock` feature. These tests
//! deliberately need no feature flag, so the default `cargo test` invocation
//! exercises real graph construction, execution, routing, durability, resume,
//! external inputs, reducers, and event streaming.

use std::sync::Arc;

use serde_json::json;
use tinyflows::graph::reducer::ClosureStateReducer;
use tinyflows::graph::{
    CollectingSink, Command, GraphBuilder, GraphEvent, GraphInput, InMemoryCheckpointer, Interrupt,
    NodeContext, NodeResult, Send,
};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct State {
    total: i64,
    log: Vec<String>,
}

#[derive(Debug)]
struct Record {
    label: String,
    amount: i64,
}

fn recording_reducer()
-> ClosureStateReducer<State, Record, impl Fn(State, Record) -> tinyflows::graph::Result<State>> {
    ClosureStateReducer::new(|mut state: State, update: Record| {
        state.total += update.amount;
        state.log.push(update.label);
        Ok(state)
    })
}

#[tokio::test]
async fn external_input_drives_parallel_send_fanout_deterministically() {
    let sink = Arc::new(CollectingSink::new());
    let graph = GraphBuilder::<State, Record>::new()
        .set_reducer(recording_reducer())
        .with_parallel(true)
        .add_node("dispatch", |_state, context: NodeContext| async move {
            let values = context
                .send_arg
                .expect("START input reaches the entry node")
                .as_array()
                .expect("input is an array")
                .iter()
                .cloned()
                .map(|value| Send::new("worker", value))
                .collect::<Vec<_>>();
            Ok(NodeResult::Command(Command::send(values)))
        })
        .add_node("worker", |_state, context: NodeContext| async move {
            assert!(context.fork.is_some(), "fanout workers carry fork identity");
            let amount = context
                .send_arg
                .expect("Send carries a value")
                .as_i64()
                .expect("worker input is an integer");
            Ok(NodeResult::Update(Record {
                label: format!("worker:{amount}"),
                amount,
            }))
        })
        .mark_command_routing("dispatch")
        .set_entry("dispatch")
        .set_finish("worker")
        .compile()
        .unwrap()
        .with_event_sink(sink.clone());

    let run = graph
        .run_with_inputs(
            State::default(),
            [GraphInput::start(json!([3, 1, 4, 1, 5]))],
        )
        .await
        .unwrap();

    assert_eq!(run.state.total, 14);
    assert_eq!(
        run.state.log,
        ["worker:3", "worker:1", "worker:4", "worker:1", "worker:5"]
    );
    assert_eq!(run.steps, 2);
    assert_eq!(
        run.visited
            .iter()
            .filter(|node| node.as_str() == "worker")
            .count(),
        5
    );
    assert_eq!(
        sink.events()
            .iter()
            .filter(|event| matches!(event, GraphEvent::ContextForked { .. }))
            .count(),
        5
    );
}

#[tokio::test]
async fn command_fanout_waiting_join_and_conditional_route_compose() {
    let graph = GraphBuilder::<State, Record>::new()
        .set_reducer(recording_reducer())
        .with_parallel(true)
        .add_node("fork", |_state, _context: NodeContext| async move {
            Ok(NodeResult::Command(Command::goto(["left", "right"])))
        })
        .add_node("left", |_state, _context: NodeContext| async move {
            Ok(NodeResult::Update(Record {
                label: "left".into(),
                amount: 2,
            }))
        })
        .add_node("right", |_state, _context: NodeContext| async move {
            Ok(NodeResult::Update(Record {
                label: "right".into(),
                amount: 3,
            }))
        })
        .add_node("join", |state, _context: NodeContext| async move {
            assert_eq!(state.total, 5, "join observes both predecessor updates");
            Ok(NodeResult::Update(Record {
                label: "joined".into(),
                amount: 0,
            }))
        })
        .add_node("accepted", |_state, _context: NodeContext| async move {
            Ok(NodeResult::Update(Record {
                label: "accepted".into(),
                amount: 10,
            }))
        })
        .add_node("rejected", |_state, _context: NodeContext| async move {
            Ok(NodeResult::Update(Record {
                label: "rejected".into(),
                amount: -10,
            }))
        })
        .set_entry("fork")
        .mark_command_routing("fork")
        .add_waiting_edge("left", "join")
        .add_waiting_edge("right", "join")
        .add_conditional_edges(
            "join",
            |state: &State| if state.total == 5 { "accept" } else { "reject" },
            [("accept", "accepted"), ("reject", "rejected")],
        )
        .set_finish("accepted")
        .set_finish("rejected")
        .compile()
        .unwrap();

    let run = graph.run(State::default()).await.unwrap();

    assert_eq!(run.state.total, 15);
    assert_eq!(run.state.log, ["left", "right", "joined", "accepted"]);
    assert_eq!(
        run.visited
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        ["fork", "left", "right", "join", "accepted"]
    );
}

#[tokio::test]
async fn interrupt_checkpoint_resume_and_history_work_as_one_flow() {
    let checkpointer = Arc::new(InMemoryCheckpointer::<State>::new());
    let sink = Arc::new(CollectingSink::new());
    let graph = GraphBuilder::<State, Record>::new()
        .set_reducer(recording_reducer())
        .add_node("approval", |_state, context: NodeContext| async move {
            match context.resume {
                None => Ok(NodeResult::Interrupt(Interrupt::with_id(
                    "approval-1",
                    "approval",
                    json!({ "question": "continue?" }),
                ))),
                Some(value) => Ok(NodeResult::Update(Record {
                    label: format!("approved:{value}"),
                    amount: 1,
                })),
            }
        })
        .add_node("finish", |state, _context: NodeContext| async move {
            assert_eq!(state.total, 1, "finish sees the resumed update");
            Ok(NodeResult::Update(Record {
                label: "finished".into(),
                amount: 1,
            }))
        })
        .set_entry("approval")
        .add_edge("approval", "finish")
        .set_finish("finish")
        .compile()
        .unwrap()
        .with_checkpointer(checkpointer.clone())
        .with_event_sink(sink.clone());

    let paused = graph
        .run_with_thread("approval-thread", State::default())
        .await
        .unwrap();
    assert!(paused.is_interrupted());
    assert_eq!(paused.interrupts[0].id, "approval-1");

    let completed = graph
        .resume("approval-thread", Command::resume(json!(true)))
        .await
        .unwrap();
    assert_eq!(completed.state.total, 2);
    assert_eq!(completed.state.log, ["approved:true", "finished"]);
    assert!(!completed.is_interrupted());

    let history = graph
        .get_state_history("approval-thread", None)
        .await
        .unwrap();
    assert!(history.len() >= 3, "pause and resumed steps are durable");
    assert_eq!(history[0].values, completed.state);
    assert!(sink.events().iter().any(|event| {
        matches!(event, GraphEvent::InterruptEmitted { interrupt } if interrupt.id == "approval-1")
    }));
    assert!(
        sink.events()
            .iter()
            .any(|event| matches!(event, GraphEvent::CheckpointRestored { .. }))
    );
}

#[tokio::test]
async fn run_with_inputs_rejects_empty_and_unknown_targets_before_execution() {
    let graph = GraphBuilder::<i32, i32>::overwrite()
        .add_node("only", |state, _context: NodeContext| async move {
            Ok(NodeResult::Update(state + 1))
        })
        .set_entry("only")
        .set_finish("only")
        .compile()
        .unwrap();

    let empty = graph.run_with_inputs(0, []).await.unwrap_err();
    assert!(empty.to_string().contains("at least one input"));

    let unknown = graph
        .run_with_inputs(0, [GraphInput::new("missing", json!(null))])
        .await
        .unwrap_err();
    assert!(unknown.to_string().contains("missing"));
}
