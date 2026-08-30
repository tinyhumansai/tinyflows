
/// A `Send` fan-out delivers a distinct per-branch argument to N parallel
/// activations of the *same* node, and the reducer merges their results.
#[tokio::test]
async fn send_fanout_delivers_distinct_args_to_parallel_branches() {
    let graph = GraphBuilder::<Counter, i32>::new()
        .set_reducer(ClosureStateReducer::new(|mut s: Counter, u: i32| {
            s.value += u;
            s.log.push(format!("worker:{u}"));
            Ok(s)
        }))
        .with_parallel(true)
        // dispatch fans out three custom inputs to the same worker node.
        .add_node("dispatch", |_s: Counter, _c: NodeContext| async move {
            Ok(NodeResult::Command(Command::send([
                Send::new("worker", json!(10)),
                Send::new("worker", json!(20)),
                Send::new("worker", json!(30)),
            ])))
        })
        // each worker invocation consumes its own send arg as the update.
        .add_node("worker", |_s: Counter, c: NodeContext| async move {
            let arg = c
                .send_arg
                .expect("worker scheduled via Send carries an arg");
            let v = arg.as_i64().unwrap() as i32;
            Ok(NodeResult::Update(v))
        })
        .mark_command_routing("dispatch")
        .set_entry("dispatch")
        .set_finish("worker")
        .compile()
        .unwrap();

    let run = graph
        .run(Counter {
            value: 0,
            log: vec![],
        })
        .await
        .unwrap();

    // All three distinct args merged: 10 + 20 + 30.
    assert_eq!(run.state.value, 60);
    // The worker ran three times (one activation per Send packet).
    let worker_runs = run
        .visited
        .iter()
        .filter(|n| n.as_str() == "worker")
        .count();
    assert_eq!(worker_runs, 3);
    let mut log = run.state.log.clone();
    log.sort();
    assert_eq!(log, vec!["worker:10", "worker:20", "worker:30"]);
}

#[tokio::test]
async fn repeated_send_activations_keep_distinct_commands() {
    // Regression: two `Send` activations of the *same* node each return a
    // distinct `Command::goto`. A node-keyed goto map let the second clobber
    // the first, so both branches routed to the survivor's target (and one
    // sink was dropped). Keyed per activation, each keeps its own routing.
    let graph = GraphBuilder::<Counter, i32>::new()
        .set_reducer(ClosureStateReducer::new(|mut s: Counter, u: i32| {
            s.value += u;
            s.log.push(format!("n:{u}"));
            Ok(s)
        }))
        .with_parallel(true)
        .add_node("dispatch", |_s: Counter, _c: NodeContext| async move {
            Ok(NodeResult::Command(Command::send([
                Send::new("worker", json!(1)),
                Send::new("worker", json!(2)),
            ])))
        })
        // Each worker routes to a different sink based on its own send arg.
        .add_node("worker", |_s: Counter, c: NodeContext| async move {
            let arg = c.send_arg.expect("worker carries a send arg");
            let target = if arg.as_i64() == Some(1) {
                "sink_a"
            } else {
                "sink_b"
            };
            Ok(NodeResult::Command(Command::new().with_goto([target])))
        })
        .add_node("sink_a", |_s: Counter, _c: NodeContext| async move {
            Ok(NodeResult::Update(10))
        })
        .add_node("sink_b", |_s: Counter, _c: NodeContext| async move {
            Ok(NodeResult::Update(20))
        })
        .mark_command_routing("dispatch")
        .mark_command_routing("worker")
        .set_entry("dispatch")
        .set_finish("sink_a")
        .set_finish("sink_b")
        .compile()
        .unwrap();

    let run = graph
        .run(Counter {
            value: 0,
            log: vec![],
        })
        .await
        .unwrap();

    // Both sinks must have run — one per activation's own goto.
    assert!(
        run.visited.iter().any(|n| n.as_str() == "sink_a"),
        "sink_a (worker arg 1's target) must run"
    );
    assert!(
        run.visited.iter().any(|n| n.as_str() == "sink_b"),
        "sink_b (worker arg 2's target) must run"
    );
    assert_eq!(run.state.value, 30, "both sinks contributed (10 + 20)");
}

#[tokio::test]
async fn run_with_inputs_seeds_start_and_peer_node() {
    let graph = GraphBuilder::<Counter, String>::new()
        .set_reducer(ClosureStateReducer::new(|mut s: Counter, u: String| {
            s.log.push(u);
            Ok(s)
        }))
        .with_parallel(true)
        .add_node("user_loop", |_s: Counter, c: NodeContext| async move {
            let input = c
                .send_arg
                .expect("start input should be delivered to entry node")
                .as_str()
                .expect("user payload is a string")
                .to_string();
            Ok(NodeResult::Update(format!("user:{input}")))
        })
        .add_node("tool_loop", |_s: Counter, c: NodeContext| async move {
            let tool = c
                .send_arg
                .expect("tool input should be delivered to peer node")
                .get("tool")
                .and_then(|v| v.as_str())
                .expect("tool payload names the tool")
                .to_string();
            Ok(NodeResult::Update(format!("tool:{tool}")))
        })
        .set_entry("user_loop")
        .set_finish("user_loop")
        .set_finish("tool_loop")
        .compile()
        .unwrap();

    let run = graph
        .run_with_inputs(
            Counter {
                value: 0,
                log: vec![],
            },
            [
                GraphInput::start(json!("hello")),
                GraphInput::new("tool_loop", json!({ "tool": "search" })),
            ],
        )
        .await
        .unwrap();

    assert_eq!(run.steps, 1);
    assert_eq!(run.state.log, vec!["user:hello", "tool:search"]);
    assert_eq!(
        run.visited.iter().map(|n| n.as_str()).collect::<Vec<_>>(),
        vec!["user_loop", "tool_loop"]
    );
}

#[tokio::test]
async fn run_with_inputs_preserves_repeated_inputs_to_same_node() {
    let graph = GraphBuilder::<Counter, String>::new()
        .set_reducer(ClosureStateReducer::new(|mut s: Counter, u: String| {
            s.log.push(u);
            Ok(s)
        }))
        .add_node("worker", |_s: Counter, c: NodeContext| async move {
            let item = c
                .send_arg
                .expect("external input should carry an item")
                .as_i64()
                .expect("item payload is an integer");
            Ok(NodeResult::Update(format!("item:{item}")))
        })
        .set_entry("worker")
        .set_finish("worker")
        .compile()
        .unwrap();

    let run = graph
        .run_with_inputs(
            Counter {
                value: 0,
                log: vec![],
            },
            [
                GraphInput::new("worker", json!(1)),
                GraphInput::new("worker", json!(2)),
                GraphInput::new("worker", json!(3)),
            ],
        )
        .await
        .unwrap();

    assert_eq!(run.steps, 1);
    assert_eq!(run.state.log, vec!["item:1", "item:2", "item:3"]);
    assert_eq!(
        run.visited
            .iter()
            .filter(|node| node.as_str() == "worker")
            .count(),
        3
    );
}

/// A node with normal `goto` (no `send_arg`) gets `None`, while the same node
/// reached via `Send` gets the packet's argument — proving the two coexist.
#[tokio::test]
async fn goto_activation_has_no_send_arg() {
    let graph = GraphBuilder::<Counter, i32>::new()
        .set_reducer(ClosureStateReducer::new(|mut s: Counter, u: i32| {
            s.value += u;
            Ok(s)
        }))
        .add_node("start", |_s: Counter, _c: NodeContext| async move {
            Ok(NodeResult::Command(Command::goto(["sink"])))
        })
        .add_node("sink", |_s: Counter, c: NodeContext| async move {
            // Plain goto activation: no per-invocation argument.
            assert!(c.send_arg.is_none());
            Ok(NodeResult::Update(1))
        })
        .mark_command_routing("start")
        .set_entry("start")
        .set_finish("sink")
        .compile()
        .unwrap();
    let run = graph
        .run(Counter {
            value: 0,
            log: vec![],
        })
        .await
        .unwrap();
    assert_eq!(run.state.value, 1);
}

/// A user route enum with `Display` can label conditional edges directly
/// (typed routes), and the [`Route`] newtype is accepted interchangeably.
#[tokio::test]
async fn typed_enum_conditional_route() {
    #[derive(Clone, Copy)]
    enum Decision {
        Approve,
        Reject,
    }
    impl std::fmt::Display for Decision {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Decision::Approve => f.write_str("approve"),
                Decision::Reject => f.write_str("reject"),
            }
        }
    }

    let graph = GraphBuilder::<i32, i32>::overwrite()
        .add_node("gate", |s: i32, _c: NodeContext| async move {
            Ok(NodeResult::Update(s))
        })
        .add_node("approved", |_s: i32, _c: NodeContext| async move {
            Ok(NodeResult::Update(100))
        })
        .add_node("rejected", |_s: i32, _c: NodeContext| async move {
            Ok(NodeResult::Update(-1))
        })
        .set_entry("gate")
        // Router returns the enum directly (impl ToString); the route table is
        // keyed by the enum variant and the `Route` newtype interchangeably.
        .add_conditional_edges(
            "gate",
            |s: &i32| {
                if *s > 0 {
                    Decision::Approve
                } else {
                    Decision::Reject
                }
            },
            [
                (Route::new(Decision::Approve), "approved"),
                (Route::new(Decision::Reject), "rejected"),
            ],
        )
        .set_finish("approved")
        .set_finish("rejected")
        .compile()
        .unwrap();

    assert_eq!(graph.run(5).await.unwrap().state, 100);
    assert_eq!(graph.run(-3).await.unwrap().state, -1);
}

/// A barrier/waiting node activates exactly once, only after *all* of its
/// registered predecessors have completed — even when they finish in different
/// supersteps.
#[tokio::test]
async fn waiting_edge_barrier_joins_staggered_predecessors() {
    let graph = GraphBuilder::<Counter, i32>::new()
        .set_reducer(ClosureStateReducer::new(|mut s: Counter, u: i32| {
            s.value += u;
            Ok(s)
        }))
        .add_node("start", |_s: Counter, _c: NodeContext| async move {
            // Fan out to a fast predecessor and a one-hop chain.
            Ok(NodeResult::Command(Command::goto(["p1", "inter"])))
        })
        .add_node("p1", |_s: Counter, _c: NodeContext| async move {
            Ok(NodeResult::Update(1))
        })
        .add_node("inter", |_s: Counter, _c: NodeContext| async move {
            Ok(NodeResult::Update(1))
        })
        .add_node("p2", |_s: Counter, _c: NodeContext| async move {
            Ok(NodeResult::Update(1))
        })
        .add_node("join", |_s: Counter, _c: NodeContext| async move {
            Ok(NodeResult::Update(10))
        })
        .mark_command_routing("start")
        .set_entry("start")
        // p1 completes in step 2; p2 only after inter (step 3). The barrier
        // holds `join` until both have arrived.
        .add_waiting_edge("p1", "join")
        .add_edge("inter", "p2")
        .add_waiting_edge("p2", "join")
        .set_finish("join")
        .compile()
        .unwrap();

    let run = graph
        .run(Counter {
            value: 0,
            log: vec![],
        })
        .await
        .unwrap();

    // join ran exactly once (not once per predecessor arrival).
    let join_runs = run.visited.iter().filter(|n| n.as_str() == "join").count();
    assert_eq!(join_runs, 1);
    // p1 + inter + p2 + join = 1 + 1 + 1 + 10.
    assert_eq!(run.state.value, 13);
    // join is the last node visited, proving it waited for both branches.
    assert_eq!(run.visited.last().unwrap().as_str(), "join");
}

/// `add_sequence` is sugar for a chain of direct edges.
#[tokio::test]
async fn add_sequence_chains_direct_edges() {
    let graph = GraphBuilder::<Counter, i32>::new()
        .set_reducer(ClosureStateReducer::new(|mut s: Counter, u: i32| {
            s.value += u;
            s.log.push(format!("+{u}"));
            Ok(s)
        }))
        .add_node("a", |_s: Counter, _c: NodeContext| async move {
            Ok(NodeResult::Update(1))
        })
        .add_node("b", |_s: Counter, _c: NodeContext| async move {
            Ok(NodeResult::Update(1))
        })
        .add_node("c", |_s: Counter, _c: NodeContext| async move {
            Ok(NodeResult::Update(1))
        })
        .set_entry("a")
        .add_sequence(["a", "b", "c"])
        .set_finish("c")
        .compile()
        .unwrap();

    let run = graph
        .run(Counter {
            value: 0,
            log: vec![],
        })
        .await
        .unwrap();
    assert_eq!(run.state.value, 3);
    assert_eq!(
        run.visited
            .iter()
            .map(|n| n.to_string())
            .collect::<Vec<_>>(),
        vec!["a", "b", "c"]
    );
}
