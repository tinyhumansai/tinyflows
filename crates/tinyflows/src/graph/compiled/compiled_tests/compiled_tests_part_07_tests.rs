
#[tokio::test]
async fn attributed_update_does_not_fire_an_unsatisfied_barrier() {
    // Diamond `super -> {b, c} -> merge` joined by waiting edges. `c` interrupts
    // on its first activation, so the pause leaves `c` pending with only `b`
    // arrived at the barrier. A manual write attributed to `b` must not
    // schedule `merge` (that would run the join without `c`'s contribution),
    // and the recorded arrival must let `merge` fire once `c` completes.
    let cp = Arc::new(InMemoryCheckpointer::<Counter>::new());
    let interrupted = Arc::new(AtomicBool::new(false));
    let once = interrupted.clone();
    let graph = GraphBuilder::<Counter, i32>::new()
        .with_parallel(true)
        .set_reducer(ClosureStateReducer::new(|mut s: Counter, u: i32| {
            s.value += u;
            s.log.push(format!("+{u}"));
            Ok(s)
        }))
        .add_node("super", |_s: Counter, _c: NodeContext| async move {
            Ok(NodeResult::Command(
                Command::default().with_goto(["b", "c"]),
            ))
        })
        .add_node("b", |_s: Counter, _c: NodeContext| async move {
            Ok(NodeResult::Update(1))
        })
        .add_node("c", move |_s: Counter, _c: NodeContext| {
            let once = once.clone();
            async move {
                if once.swap(true, AtomicOrdering::SeqCst) {
                    Ok(NodeResult::Update(2))
                } else {
                    Ok(NodeResult::Interrupt(Interrupt::new("c", json!({}))))
                }
            }
        })
        .add_node("merge", |_s: Counter, _c: NodeContext| async move {
            Ok(NodeResult::Update(100))
        })
        .set_entry("super")
        .mark_command_routing("super")
        .add_waiting_edge("b", "merge")
        .add_waiting_edge("c", "merge")
        .set_finish("merge")
        .compile()
        .unwrap()
        .with_checkpointer(cp.clone());

    let paused = graph
        .run_with_thread(
            "t-barrier-update",
            Counter {
                value: 0,
                log: vec![],
            },
        )
        .await
        .unwrap();
    assert!(paused.is_interrupted());

    // Operator edits state and attributes the write to `b`, the barrier
    // predecessor that already ran.
    graph
        .update_state("t-barrier-update", 10, Some(NodeId::from("b")))
        .await
        .unwrap();
    let written = cp.get("t-barrier-update", None).await.unwrap().unwrap();
    assert!(
        !written.next_nodes.iter().any(|n| n.as_str() == "merge"),
        "an unsatisfied barrier must not be scheduled by an attributed write"
    );
    assert!(
        written.next_nodes.iter().any(|n| n.as_str() == "c"),
        "the still-pending barrier predecessor must stay scheduled"
    );
    // Resume prefers `pending_activations` over `next_nodes`, so the two must
    // never disagree: a node named by only one of them would be silently
    // dropped (or scheduled without its `Send` arg).
    if let Some(pending) = &written.pending_activations {
        assert_eq!(
            pending.iter().map(|a| a.node.clone()).collect::<Vec<_>>(),
            written.next_nodes,
            "pending activations and next nodes must describe the same schedule"
        );
    }

    let done = graph.retry("t-barrier-update").await.unwrap();
    assert!(
        done.visited.iter().any(|n| n.as_str() == "merge"),
        "merge must fire once the remaining predecessor arrives"
    );
    // 1 (b) + 10 (manual write) + 2 (c) + 100 (merge).
    assert_eq!(done.state.value, 113);
}

/// Fan-out graph used by the attributed-write scheduling tests: `super` forks
/// into `b -> x -> y` and `c`, where `c` interrupts on its first activation.
/// The pause therefore leaves a checkpoint with two independent pending
/// branches (`x` from the completed `b`, and the still-interrupted `c`).
fn forked_interrupt_graph(
    cp: Arc<InMemoryCheckpointer<Counter>>,
    interrupted: Arc<AtomicBool>,
) -> CompiledGraph<Counter, i32> {
    GraphBuilder::<Counter, i32>::new()
        .with_parallel(true)
        .set_reducer(ClosureStateReducer::new(|mut s: Counter, u: i32| {
            s.value += u;
            s.log.push(format!("+{u}"));
            Ok(s)
        }))
        .add_node("super", |_s: Counter, _c: NodeContext| async move {
            Ok(NodeResult::Command(
                Command::default().with_goto(["b", "c"]),
            ))
        })
        .add_node("b", |_s: Counter, _c: NodeContext| async move {
            Ok(NodeResult::Update(1))
        })
        .add_node("x", |_s: Counter, _c: NodeContext| async move {
            Ok(NodeResult::Update(20))
        })
        .add_node("y", |_s: Counter, _c: NodeContext| async move {
            Ok(NodeResult::Update(40))
        })
        .add_node("c", move |_s: Counter, _c: NodeContext| {
            let once = interrupted.clone();
            async move {
                if once.swap(true, AtomicOrdering::SeqCst) {
                    Ok(NodeResult::Update(2))
                } else {
                    Ok(NodeResult::Interrupt(Interrupt::new("c", json!({}))))
                }
            }
        })
        .set_entry("super")
        .mark_command_routing("super")
        .add_sequence(["b", "x", "y"])
        .set_finish("y")
        .set_finish("c")
        .compile()
        .unwrap()
        .with_checkpointer(cp)
}

#[tokio::test]
async fn attributed_update_keeps_other_pending_branches_scheduled() {
    // Two independent branches are pending (`x` and the interrupted `c`). A
    // manual write attributed to `x` schedules x's successor `y`, but it must
    // not discard `c`: the attributed node's successors *add to* the schedule
    // rather than replacing it, or the untouched branch is silently dropped and
    // never runs again.
    let cp = Arc::new(InMemoryCheckpointer::<Counter>::new());
    let graph = forked_interrupt_graph(cp.clone(), Arc::new(AtomicBool::new(false)));

    let paused = graph
        .run_with_thread(
            "t-fork-update",
            Counter {
                value: 0,
                log: vec![],
            },
        )
        .await
        .unwrap();
    assert!(paused.is_interrupted());

    let before = cp.get("t-fork-update", None).await.unwrap().unwrap();
    assert!(
        before.next_nodes.iter().any(|n| n.as_str() == "x")
            && before.next_nodes.iter().any(|n| n.as_str() == "c"),
        "precondition: both branches pending, got {:?}",
        before.next_nodes
    );

    graph
        .update_state("t-fork-update", 10, Some(NodeId::from("x")))
        .await
        .unwrap();
    let written = cp.get("t-fork-update", None).await.unwrap().unwrap();
    assert!(
        written.next_nodes.iter().any(|n| n.as_str() == "y"),
        "the attributed node's successor must be scheduled, got {:?}",
        written.next_nodes
    );
    assert!(
        written.next_nodes.iter().any(|n| n.as_str() == "c"),
        "the untouched pending branch must stay scheduled, got {:?}",
        written.next_nodes
    );
    assert!(
        !written.next_nodes.iter().any(|n| n.as_str() == "x"),
        "the attributed node itself is completed, not pending: {:?}",
        written.next_nodes
    );
    // Resume prefers `pending_activations` over `next_nodes`, so the two must
    // never disagree.
    if let Some(pending) = &written.pending_activations {
        assert_eq!(
            pending.iter().map(|a| a.node.clone()).collect::<Vec<_>>(),
            written.next_nodes,
            "pending activations and next nodes must describe the same schedule"
        );
    }

    let done = graph.retry("t-fork-update").await.unwrap();
    assert!(
        done.visited.iter().any(|n| n.as_str() == "c"),
        "the dropped branch must still run, visited {:?}",
        done.visited
    );
    // 1 (b) + 10 (manual write) + 2 (c) + 40 (y).
    assert_eq!(done.state.value, 53);
}

#[tokio::test]
async fn attributed_update_to_sink_node_keeps_other_pending_branches() {
    // `c` is terminal, so an attributed write to it schedules nothing of its
    // own. Replacing the pending set with that empty routing would drop the
    // sibling `x` branch *and* leave a checkpoint with nothing to resume.
    let cp = Arc::new(InMemoryCheckpointer::<Counter>::new());
    let graph = forked_interrupt_graph(cp.clone(), Arc::new(AtomicBool::new(false)));

    let paused = graph
        .run_with_thread(
            "t-fork-sink",
            Counter {
                value: 0,
                log: vec![],
            },
        )
        .await
        .unwrap();
    assert!(paused.is_interrupted());

    graph
        .update_state("t-fork-sink", 10, Some(NodeId::from("c")))
        .await
        .unwrap();
    let written = cp.get("t-fork-sink", None).await.unwrap().unwrap();
    assert_eq!(
        written
            .next_nodes
            .iter()
            .map(|n| n.to_string())
            .collect::<Vec<_>>(),
        vec!["x".to_string()],
        "the sibling branch must survive an attributed write to a sink node"
    );

    let done = graph.retry("t-fork-sink").await.unwrap();
    // 1 (b) + 10 (manual write) + 20 (x) + 40 (y); `c` is attributed as done.
    assert_eq!(done.state.value, 71);
}

#[tokio::test]
async fn attributed_update_preserves_pending_send_args_of_other_branches() {
    // Three `Send` activations of `worker` run concurrently; the one carrying
    // arg 1 interrupts, the other two finish. A write attributed to an unrelated
    // node must carry the still-pending packet over *with* its arg — dropping it
    // loses the fanout, and re-scheduling by node id alone loses the payload.
    //
    // Only the interrupted packet is pending. Workers 2 and 3 ran to completion
    // in this same superstep (parallel execution folds the whole active set), so
    // their updates are already committed and rescheduling them would run them a
    // second time. Their work is asserted against the state below rather than
    // against the pending set.
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
        .add_node("side", |_s: Counter, _c: NodeContext| async move {
            Ok(NodeResult::Update(0))
        })
        .add_node("tail", |_s: Counter, _c: NodeContext| async move {
            Ok(NodeResult::Update(0))
        })
        .set_entry("dispatch")
        .mark_command_routing("dispatch")
        .add_edge("side", "tail")
        .set_finish("worker")
        .set_finish("tail")
        .compile()
        .unwrap()
        .with_checkpointer(cp.clone());

    let paused = graph
        .run_with_thread(
            "t-send-update",
            Counter {
                value: 0,
                log: vec![],
            },
        )
        .await
        .unwrap();
    assert!(paused.is_interrupted());

    graph
        .update_state("t-send-update", 0, Some(NodeId::from("side")))
        .await
        .unwrap();
    let written = cp.get("t-send-update", None).await.unwrap().unwrap();
    let pending = written
        .pending_activations
        .clone()
        .expect("an attributed write must persist the merged activations");
    let mut args: Vec<i64> = pending
        .iter()
        .filter(|a| a.node.as_str() == "worker")
        .map(|a| {
            a.send_arg
                .as_ref()
                .expect("pending Send activations keep their arg")
                .as_i64()
                .unwrap()
        })
        .collect();
    args.sort_unstable();
    assert_eq!(
        args,
        vec![1],
        "only the interrupted packet is pending, and it keeps its arg"
    );
    assert_eq!(
        paused.state.value, 5,
        "workers 2 and 3 completed in the interrupted step, so their updates are \
         already folded (2 + 3) — a lower value means completed work was thrown away"
    );
    assert!(
        pending.iter().any(|a| a.node.as_str() == "tail"),
        "the attributed node's successor is scheduled alongside them"
    );
    assert_eq!(
        pending.iter().map(|a| a.node.clone()).collect::<Vec<_>>(),
        written.next_nodes,
        "pending activations and next nodes must describe the same schedule"
    );
}
