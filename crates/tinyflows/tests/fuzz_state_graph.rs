//! Property tests for the public state-graph runtime.
//!
//! Generated cases exercise topology depth, dynamic fanout width and values,
//! parallel/sequential equivalence, conditional routing, and durable resume.

use std::sync::Arc;

use proptest::prelude::*;
use serde_json::json;
use tinyflows::graph::reducer::ClosureStateReducer;
use tinyflows::graph::{
    Command, GraphBuilder, InMemoryCheckpointer, Interrupt, NodeContext, NodeResult, Send,
};

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build property-test runtime")
}

fn fanout_graph(parallel: bool) -> tinyflows::graph::CompiledGraph<Vec<i64>, i64> {
    GraphBuilder::<Vec<i64>, i64>::new()
        .set_reducer(ClosureStateReducer::new(|mut state: Vec<i64>, update| {
            state.push(update);
            Ok(state)
        }))
        .with_parallel(parallel)
        .add_node("dispatch", |_state, context: NodeContext| async move {
            let values = context.send_arg.unwrap().as_array().unwrap().clone();
            Ok(NodeResult::Command(Command::send(
                values.into_iter().map(|value| Send::new("worker", value)),
            )))
        })
        .add_node("worker", |_state, context: NodeContext| async move {
            Ok(NodeResult::Update(
                context.send_arg.unwrap().as_i64().unwrap(),
            ))
        })
        .mark_command_routing("dispatch")
        .set_entry("dispatch")
        .set_finish("worker")
        .compile()
        .unwrap()
}

fn run_fanout(values: &[i16], parallel: bool) -> Vec<i64> {
    let payload = values.iter().map(|value| json!(*value)).collect::<Vec<_>>();
    runtime().block_on(async {
        fanout_graph(parallel)
            .run_with_inputs(
                Vec::new(),
                [tinyflows::graph::GraphInput::start(json!(payload))],
            )
            .await
            .unwrap()
            .state
    })
}

fn linear_graph(weights: &[i16]) -> tinyflows::graph::CompiledGraph<i64, i64> {
    let mut builder = GraphBuilder::<i64, i64>::new().set_reducer(ClosureStateReducer::new(
        |state: i64, update: i64| Ok(state + update),
    ));
    for (index, weight) in weights.iter().copied().enumerate() {
        builder = builder.add_node(
            format!("n{index}"),
            move |_state, _context: NodeContext| async move {
                Ok(NodeResult::Update(i64::from(weight)))
            },
        );
    }
    builder = builder.set_entry("n0");
    for index in 1..weights.len() {
        builder = builder.add_edge(format!("n{}", index - 1), format!("n{index}"));
    }
    builder
        .set_finish(format!("n{}", weights.len() - 1))
        .compile()
        .unwrap()
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 128, ..ProptestConfig::default() })]

    #[test]
    fn generated_linear_graphs_visit_every_node_once(weights in prop::collection::vec(any::<i16>(), 1..40)) {
        let expected = weights.iter().map(|value| i64::from(*value)).sum::<i64>();
        let run = runtime().block_on(linear_graph(&weights).run(0)).unwrap();

        prop_assert_eq!(run.state, expected);
        prop_assert_eq!(run.steps, weights.len());
        prop_assert_eq!(run.visited.len(), weights.len());
        for (index, node) in run.visited.iter().enumerate() {
            prop_assert_eq!(node.as_str(), format!("n{index}"));
        }
    }

    #[test]
    fn dynamic_fanout_preserves_values_and_parallel_matches_sequential(
        values in prop::collection::vec(any::<i16>(), 0..48)
    ) {
        let expected = values.iter().map(|value| i64::from(*value)).collect::<Vec<_>>();
        let sequential = run_fanout(&values, false);
        let parallel = run_fanout(&values, true);

        prop_assert_eq!(&sequential, &expected);
        prop_assert_eq!(&parallel, &expected);
        prop_assert_eq!(parallel, sequential);
    }

    #[test]
    fn conditional_routes_select_exactly_one_branch(seed in any::<i32>()) {
        let graph = GraphBuilder::<Vec<String>, String>::new()
            .set_reducer(ClosureStateReducer::new(|mut state: Vec<String>, update| {
                state.push(update);
                Ok(state)
            }))
            .add_node("route", move |_state, _context: NodeContext| async move {
                Ok(NodeResult::Update(format!("seed:{seed}")))
            })
            .add_node("even", |_state, _context: NodeContext| async move {
                Ok(NodeResult::Update("even".to_string()))
            })
            .add_node("odd", |_state, _context: NodeContext| async move {
                Ok(NodeResult::Update("odd".to_string()))
            })
            .set_entry("route")
            .add_conditional_edges(
                "route",
                move |_state: &Vec<String>| if seed % 2 == 0 { "even" } else { "odd" },
                [("even", "even"), ("odd", "odd")],
            )
            .set_finish("even")
            .set_finish("odd")
            .compile()
            .unwrap();
        let run = runtime().block_on(graph.run(Vec::new())).unwrap();
        let selected = if seed % 2 == 0 { "even" } else { "odd" };
        let rejected = if seed % 2 == 0 { "odd" } else { "even" };

        prop_assert_eq!(run.state, vec![format!("seed:{seed}"), selected.to_string()]);
        prop_assert!(run.visited.iter().any(|node| node.as_str() == selected));
        prop_assert!(!run.visited.iter().any(|node| node.as_str() == rejected));
    }

    #[test]
    fn arbitrary_resume_values_survive_interrupt_checkpointing(value in any::<i64>()) {
        let checkpointer = Arc::new(InMemoryCheckpointer::<Vec<i64>>::new());
        let graph = GraphBuilder::<Vec<i64>, i64>::new()
            .set_reducer(ClosureStateReducer::new(|mut state: Vec<i64>, update| {
                state.push(update);
                Ok(state)
            }))
            .add_node("gate", |_state, context: NodeContext| async move {
                match context.resume {
                    Some(value) => Ok(NodeResult::Update(value.as_i64().unwrap())),
                    None => Ok(NodeResult::Interrupt(Interrupt::new("gate", json!("value")))),
                }
            })
            .set_entry("gate")
            .set_finish("gate")
            .compile()
            .unwrap()
            .with_checkpointer(checkpointer);

        let resumed = runtime().block_on(async {
            let paused = graph.run_with_thread("fuzz-resume", Vec::new()).await.unwrap();
            assert!(paused.is_interrupted());
            graph.resume("fuzz-resume", Command::resume(json!(value))).await.unwrap()
        });
        prop_assert_eq!(&resumed.state, &vec![value]);
        prop_assert!(!resumed.is_interrupted());
    }
}
