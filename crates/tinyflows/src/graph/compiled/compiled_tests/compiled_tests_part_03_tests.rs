
#[tokio::test]
async fn parallel_runs_branches_concurrently_and_merges() {
    let conc = Concurrency::new();
    let graph = fanout_graph(true, conc.clone());
    let run = graph.run(Fan::default()).await.unwrap();

    // All three branches ran at the same time.
    assert_eq!(conc.max_observed(), 3);
    // Reducer merged every branch's contribution.
    assert_eq!(run.state.values, vec![1, 2, 4]);
    // Fork indices are deterministic active-set positions, not completion order.
    assert_eq!(run.state.forks, vec![0, 1, 2]);
    // Downstream join observed the merged state.
    assert_eq!(run.state.joined_sum, Some(7));
    // super | (a,b,c) | join == 3 supersteps.
    assert_eq!(run.steps, 3);
}

#[tokio::test]
async fn sequential_mode_runs_one_branch_at_a_time() {
    let conc = Concurrency::new();
    let graph = fanout_graph(false, conc.clone());
    let run = graph.run(Fan::default()).await.unwrap();

    // Never more than one branch in flight in sequential mode.
    assert_eq!(conc.max_observed(), 1);
    // Same deterministic merge as the parallel run.
    assert_eq!(run.state.values, vec![1, 2, 4]);
    assert_eq!(run.state.joined_sum, Some(7));
    // Sequential branches get no fork identity.
    assert_eq!(run.state.forks, vec![usize::MAX, usize::MAX, usize::MAX]);
    assert_eq!(run.steps, 3);
}

#[tokio::test]
async fn parallel_merge_is_reproducible() {
    // Run the same parallel fan-out repeatedly; the merged order must be stable
    // regardless of which branch's sleep finishes first.
    for _ in 0..5 {
        let graph = fanout_graph(true, Concurrency::new());
        let run = graph.run(Fan::default()).await.unwrap();
        assert_eq!(run.state.values, vec![1, 2, 4]);
        assert_eq!(run.state.forks, vec![0, 1, 2]);
        assert_eq!(run.state.joined_sum, Some(7));
    }
}

#[tokio::test]
async fn recursion_limit_is_deterministic_in_parallel() {
    // A self-looping fan-out in parallel mode must still hit the recursion limit
    // deterministically at the configured number of supersteps.
    let graph = GraphBuilder::<i32, i32>::overwrite()
        .with_parallel(true)
        .with_recursion_limit(3)
        .add_node("loop", |s, _c: NodeContext| async move {
            Ok(NodeResult::Command(
                Command::update(s + 1).with_goto(["loop"]),
            ))
        })
        .set_entry("loop")
        .mark_command_routing("loop")
        .compile()
        .unwrap();

    let err = graph.run(0).await.unwrap_err();
    assert!(matches!(err, GraphError::RecursionLimit(3)));
}

#[tokio::test]
async fn parallel_interrupt_pauses_at_lowest_index_branch() {
    // When a parallel branch interrupts, the step pauses; the interrupted branch
    // and every later active node become the checkpoint's pending nodes, while
    // lower-index successful branches' updates are still applied.
    let cp = Arc::new(InMemoryCheckpointer::<Fan>::new());
    let graph = GraphBuilder::<Fan, FanUpdate>::new()
        .with_parallel(true)
        .set_reducer(ClosureStateReducer::new(|mut s: Fan, u: FanUpdate| {
            if let FanUpdate::Branch { value, fork } = u {
                s.values.push(value);
                s.forks.push(fork);
            }
            Ok(s)
        }))
        .add_node("super", |_s: Fan, _c: NodeContext| async move {
            Ok(NodeResult::Command(
                Command::default().with_goto(["a", "b"]),
            ))
        })
        .add_node("a", |_s: Fan, _c: NodeContext| async move {
            Ok(NodeResult::Update(FanUpdate::Branch { value: 1, fork: 0 }))
        })
        .add_node("b", |_s: Fan, _c: NodeContext| async move {
            Ok(NodeResult::Interrupt(Interrupt::new("b", json!({}))))
        })
        .set_entry("super")
        .mark_command_routing("super")
        .set_finish("a")
        .set_finish("b")
        .compile()
        .unwrap()
        .with_checkpointer(cp.clone());

    let paused = graph.run_with_thread("fan", Fan::default()).await.unwrap();
    assert!(paused.is_interrupted());
    // Lower-index branch `a` committed before the pause.
    assert_eq!(paused.state.values, vec![1]);
    // The interrupting branch `b` is the head of the pending set.
    assert_eq!(
        paused.status.active_nodes.first().map(|n| n.to_string()),
        Some("b".to_string())
    );
}

#[tokio::test]
async fn parallel_interrupt_schedules_completed_branch_successors() {
    // Parallel [a, b]: a routes to successor `x` and completes; b interrupts.
    // After resume, x (a's successor) must still run — its scheduling used to be
    // dropped at the interrupt boundary, so x silently never executed.
    let cp = Arc::new(InMemoryCheckpointer::<Counter>::new());
    let graph = GraphBuilder::<Counter, i32>::new()
        .with_parallel(true)
        .set_reducer(ClosureStateReducer::new(|mut s: Counter, u: i32| {
            s.value += u;
            s.log.push(format!("+{u}"));
            Ok(s)
        }))
        .add_node("super", |_s: Counter, _c: NodeContext| async move {
            Ok(NodeResult::Command(
                Command::default().with_goto(["a", "b"]),
            ))
        })
        .add_node("a", |_s: Counter, _c: NodeContext| async move {
            Ok(NodeResult::Update(1))
        })
        .add_node("b", |_s: Counter, c: NodeContext| async move {
            match c.resume {
                Some(_) => Ok(NodeResult::Update(100)),
                None => Ok(NodeResult::Interrupt(Interrupt::new("b", json!({})))),
            }
        })
        .add_node("x", |_s: Counter, _c: NodeContext| async move {
            Ok(NodeResult::Update(10))
        })
        .set_entry("super")
        .mark_command_routing("super")
        .add_edge("a", "x")
        .set_finish("b")
        .set_finish("x")
        .compile()
        .unwrap()
        .with_checkpointer(cp.clone());

    let paused = graph
        .run_with_thread(
            "t",
            Counter {
                value: 0,
                log: vec![],
            },
        )
        .await
        .unwrap();
    assert!(paused.is_interrupted());
    assert_eq!(paused.state.value, 1, "branch a committed before the pause");

    let done = graph
        .resume("t", Command::resume(json!(null)))
        .await
        .unwrap();
    assert!(
        done.visited.iter().any(|n| n.as_str() == "x"),
        "a's successor x must run after resume"
    );
    // 1 (a) + 100 (b resume) + 10 (x) — every scheduled branch ran once.
    assert_eq!(done.state.value, 111);
}

#[tokio::test]
async fn send_args_survive_interrupt_and_resume() {
    // A `Send` fanout schedules three workers (args 1, 2, 3); the arg-1 worker
    // interrupts on its first activation. On resume every pending worker must
    // still carry its own send arg — before the fix they resumed with `None`.
    let cp = Arc::new(InMemoryCheckpointer::<Counter>::new());
    let graph = GraphBuilder::<Counter, i32>::new()
        .with_parallel(true)
        .set_reducer(ClosureStateReducer::new(|mut s: Counter, u: i32| {
            s.value += u;
            s.log.push(format!("w:{u}"));
            Ok(s)
        }))
        .add_node("dispatch", |_s: Counter, _c: NodeContext| async move {
            Ok(NodeResult::Command(Command::send([
                Send::new("worker", json!(1)),
                Send::new("worker", json!(2)),
                Send::new("worker", json!(3)),
            ])))
        })
        .add_node("worker", |_s: Counter, c: NodeContext| async move {
            let arg = c
                .send_arg
                .clone()
                .expect("worker scheduled via Send must carry its arg")
                .as_i64()
                .unwrap() as i32;
            if arg == 1 && c.resume.is_none() {
                return Ok(NodeResult::Interrupt(Interrupt::new("worker", json!({}))));
            }
            Ok(NodeResult::Update(arg))
        })
        .set_entry("dispatch")
        .mark_command_routing("dispatch")
        .set_finish("worker")
        .compile()
        .unwrap()
        .with_checkpointer(cp.clone());

    let paused = graph
        .run_with_thread(
            "fan",
            Counter {
                value: 0,
                log: vec![],
            },
        )
        .await
        .unwrap();
    assert!(paused.is_interrupted());

    // Resume: the arg-1 worker unblocks and the other two re-run with their
    // preserved args. With the arg lost, `expect(...)` above would panic.
    let done = graph
        .resume("fan", Command::resume(json!(null)))
        .await
        .unwrap();
    assert_eq!(done.state.value, 6, "all three worker args (1+2+3) applied");
    let mut log = done.state.log.clone();
    log.sort();
    assert_eq!(log, vec!["w:1", "w:2", "w:3"]);
}

#[tokio::test]
async fn barrier_arrivals_survive_interrupt_and_resume() {
    // Diamond join: p1 arrives at the barrier before an interrupt; p2 arrives
    // only after resume. The join must still fire — the p1 arrival has to
    // survive the checkpoint boundary or the join's precondition is never met.
    let cp = Arc::new(InMemoryCheckpointer::<Counter>::new());
    let graph = GraphBuilder::<Counter, i32>::new()
        .with_parallel(true)
        .set_reducer(ClosureStateReducer::new(|mut s: Counter, u: i32| {
            s.value += u;
            s.log.push(format!("+{u}"));
            Ok(s)
        }))
        .add_node("super", |_s: Counter, _c: NodeContext| async move {
            Ok(NodeResult::Command(
                Command::default().with_goto(["p1", "hold"]),
            ))
        })
        .add_node("p1", |_s: Counter, _c: NodeContext| async move {
            Ok(NodeResult::Update(1))
        })
        .add_node("p2", |_s: Counter, _c: NodeContext| async move {
            Ok(NodeResult::Update(2))
        })
        // `hold` interrupts first; on resume it routes to p2 (the second
        // barrier predecessor).
        .add_node("hold", |_s: Counter, c: NodeContext| async move {
            match c.resume {
                Some(_) => Ok(NodeResult::Command(Command::new().with_goto(["p2"]))),
                None => Ok(NodeResult::Interrupt(Interrupt::new("hold", json!({})))),
            }
        })
        .add_node("join", |_s: Counter, _c: NodeContext| async move {
            Ok(NodeResult::Update(100))
        })
        .set_entry("super")
        .mark_command_routing("super")
        .mark_command_routing("hold")
        .add_waiting_edge("p1", "join")
        .add_waiting_edge("p2", "join")
        .set_finish("join")
        .compile()
        .unwrap()
        .with_checkpointer(cp.clone());

    let paused = graph
        .run_with_thread(
            "diamond",
            Counter {
                value: 0,
                log: vec![],
            },
        )
        .await
        .unwrap();
    assert!(paused.is_interrupted());
    assert_eq!(
        paused.state.value, 1,
        "p1 committed (arrived at the barrier)"
    );

    let done = graph
        .resume("diamond", Command::resume(json!(null)))
        .await
        .unwrap();
    assert!(
        done.visited.iter().any(|n| n.as_str() == "join"),
        "join must fire once both barrier predecessors have arrived across the resume"
    );
    // 1 (p1) + 2 (p2) + 100 (join).
    assert_eq!(done.state.value, 103);
}

#[tokio::test]
async fn barrier_relief_fires_when_source_skips_relief_node() {
    // Mixed fan-in: `m` waits on both `a` and `c`. `condition` never routes to
    // `a` — it always takes the `skip` route to END, simulating an untaken
    // conditional branch — so without a barrier relief `m` would deadlock
    // forever waiting on a predecessor that never runs.
    // `add_barrier_relief("condition", "a", "m")` registers `a`'s phantom
    // arrival at `m` whenever `condition` completes without activating `a`,
    // so `m` still fires once `c`'s real arrival lands.
    let graph = GraphBuilder::<Vec<String>, String>::new()
        .with_parallel(true)
        .set_reducer(ClosureStateReducer::new(|mut s: Vec<String>, u: String| {
            s.push(u);
            Ok(s)
        }))
        .add_node("start", |_s, _c: NodeContext| async move {
            Ok(NodeResult::Command(
                Command::default().with_goto(["condition", "c"]),
            ))
        })
        .add_node("condition", |_s, _c: NodeContext| async move {
            Ok(NodeResult::Update("condition".to_string()))
        })
        .add_node("a", |_s, _c: NodeContext| async move {
            Ok(NodeResult::Update("a".to_string()))
        })
        .add_node("c", |_s, _c: NodeContext| async move {
            Ok(NodeResult::Update("c".to_string()))
        })
        .add_node("m", |_s, _c: NodeContext| async move {
            Ok(NodeResult::Update("m".to_string()))
        })
        .set_entry("start")
        .mark_command_routing("start")
        // `condition` always takes the `skip` route to END — `a` is never
        // reached via a real edge.
        .add_conditional_edges(
            "condition",
            |_s: &Vec<String>| "skip".to_string(),
            [("skip", END)],
        )
        .add_waiting_edge("a", "m")
        .add_waiting_edge("c", "m")
        .add_barrier_relief("condition", "a", "m")
        .set_finish("m")
        .compile()
        .unwrap();

    let run = graph.run(Vec::new()).await.unwrap();

    assert!(
        run.visited.iter().any(|n| n.as_str() == "m"),
        "m must activate via the barrier relief even though `a` never ran"
    );
    assert!(
        !run.visited.iter().any(|n| n.as_str() == "a"),
        "a must never have run (condition always skipped it)"
    );
    assert_eq!(
        run.state,
        vec!["condition".to_string(), "c".to_string(), "m".to_string()],
        "m fires off condition+c's real contributions, with no phantom `a` update"
    );
}

#[tokio::test]
async fn reducer_error_at_boundary_transitions_run_to_failed() {
    // A reducer error raised at the step boundary (after the node ran) must
    // still fail the run — emit RunFailed / a Failed status — rather than
    // unwinding and leaving observers to see the run stuck in Running.
    let sink = Arc::new(CollectingSink::new());
    let graph = GraphBuilder::<i32, i32>::new()
        .set_reducer(ClosureStateReducer::new(|_s: i32, u: i32| {
            if u == 999 {
                Err(GraphError::Graph("reducer boom".to_string()))
            } else {
                Ok(u)
            }
        }))
        .add_node("boom", |_s, _c: NodeContext| async move {
            Ok(NodeResult::Update(999))
        })
        .set_entry("boom")
        .set_finish("boom")
        .compile()
        .unwrap()
        .with_event_sink(sink.clone());

    let err = graph.run(0).await.unwrap_err();
    assert!(matches!(err, GraphError::Graph(_)), "got {err:?}");
    assert!(
        sink.events()
            .iter()
            .any(|e| matches!(e, GraphEvent::RunFailed { .. })),
        "a boundary reducer error must transition the run to Failed (RunFailed emitted)"
    );
}

#[tokio::test]
async fn status_snapshot_reports_run() {
    let graph = adding_graph();
    let run = graph
        .run(Counter {
            value: 0,
            log: vec![],
        })
        .await
        .unwrap();
    let status = &run.status;
    assert_eq!(status.status, ExecutionStatus::Completed);
    assert_eq!(status.current_step, 2);
    assert!(status.ended_at.is_some());
    assert!(status.error.is_none());
    assert_eq!(status.graph_id, *graph.graph_id());
}
