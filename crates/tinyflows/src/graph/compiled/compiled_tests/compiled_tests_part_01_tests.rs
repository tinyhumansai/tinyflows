

#[tokio::test]
async fn partial_updates_and_reducer() {
    let graph = adding_graph();
    let run = graph
        .run(Counter {
            value: 0,
            log: vec![],
        })
        .await
        .unwrap();
    // inc -> value=1 ; double -> +1 (value snapshot) -> value=2
    assert_eq!(run.state.value, 2);
    assert_eq!(run.state.log, vec!["+1", "+1"]);
    assert_eq!(run.steps, 2);
    assert_eq!(
        run.visited
            .iter()
            .map(|n| n.to_string())
            .collect::<Vec<_>>(),
        vec!["inc", "double"]
    );
    assert_eq!(run.status.status, ExecutionStatus::Completed);
    assert!(!run.is_interrupted());
}

#[tokio::test]
async fn conditional_routing_selects_branch() {
    let graph = GraphBuilder::<i32, i32>::overwrite()
        .add_node("start", |_s, _c: NodeContext| async move {
            Ok(NodeResult::Update(0))
        })
        .add_node("even", |_s, _c: NodeContext| async move {
            Ok(NodeResult::Update(100))
        })
        .add_node("odd", |_s, _c: NodeContext| async move {
            Ok(NodeResult::Update(200))
        })
        .set_entry("start")
        .add_conditional_edges(
            "start",
            |s: &i32| {
                if *s % 2 == 0 {
                    "even".to_string()
                } else {
                    "odd".to_string()
                }
            },
            [("even", "even"), ("odd", "odd")],
        )
        .set_finish("even")
        .set_finish("odd")
        .compile()
        .unwrap();

    let run = graph.run(0).await.unwrap();
    assert_eq!(run.state, 100);
}

#[tokio::test]
async fn command_goto_overrides_edges() {
    let graph = GraphBuilder::<i32, i32>::overwrite()
        .add_node("router", |_s, _c: NodeContext| async move {
            Ok(NodeResult::Command(
                Command::update(5).with_goto(["target"]),
            ))
        })
        .add_node("target", |s, _c: NodeContext| async move {
            Ok(NodeResult::Update(s + 1))
        })
        .set_entry("router")
        .mark_command_routing("router")
        .set_finish("target")
        .compile()
        .unwrap();

    let run = graph.run(0).await.unwrap();
    assert_eq!(run.state, 6);
}

#[tokio::test]
async fn command_goto_rejects_unknown_target_immediately() {
    let graph = GraphBuilder::<i32, i32>::overwrite()
        .add_node("router", |_s, _c: NodeContext| async move {
            Ok(NodeResult::Command(Command::goto(["missing"])))
        })
        .set_entry("router")
        .mark_command_routing("router")
        .compile()
        .unwrap();

    let err = graph.run(0).await.unwrap_err();
    match err {
        GraphError::MissingNode(node) => assert_eq!(node, "missing"),
        other => panic!("expected MissingNode, got {other:?}"),
    }
}

#[tokio::test]
async fn command_goto_rejects_start_target() {
    let graph = GraphBuilder::<i32, i32>::overwrite()
        .add_node("router", |_s, _c: NodeContext| async move {
            Ok(NodeResult::Command(Command::goto(["__start__"])))
        })
        .set_entry("router")
        .mark_command_routing("router")
        .compile()
        .unwrap();

    let err = graph.run(0).await.unwrap_err();
    assert!(matches!(err, GraphError::Graph(_)), "got {err:?}");
    assert!(err.to_string().contains("START"), "{err}");
}

#[tokio::test]
async fn invalid_command_goto_is_not_persisted_as_next_node() {
    let cp = Arc::new(InMemoryCheckpointer::<i32>::new());
    let graph = GraphBuilder::<i32, i32>::overwrite()
        .add_node("router", |_s, _c: NodeContext| async move {
            Ok(NodeResult::Command(
                Command::update(1).with_goto(["missing"]),
            ))
        })
        .set_entry("router")
        .mark_command_routing("router")
        .compile()
        .unwrap()
        .with_checkpointer(cp.clone());

    let err = graph.run_with_thread("bad-goto", 0).await.unwrap_err();
    assert!(matches!(err, GraphError::MissingNode(_)), "got {err:?}");
    assert_eq!(
        cp.count("bad-goto"),
        0,
        "invalid runtime route must fail before boundary checkpoint persistence"
    );
}

#[tokio::test]
async fn recursion_limit_is_deterministic() {
    let graph = GraphBuilder::<i32, i32>::overwrite()
        .with_recursion_limit(3)
        .add_node("loop", |s, _c: NodeContext| async move {
            Ok(NodeResult::Update(s + 1))
        })
        .set_entry("loop")
        .add_edge("loop", "loop")
        .compile()
        .unwrap();

    let err = graph.run(0).await.unwrap_err();
    assert!(matches!(err, GraphError::RecursionLimit(3)));
}

#[tokio::test]
async fn superstep_count_matches_path_length() {
    let graph = GraphBuilder::<i32, i32>::overwrite()
        .add_node("a", |s, _c: NodeContext| async move {
            Ok(NodeResult::Update(s + 1))
        })
        .add_node("b", |s, _c: NodeContext| async move {
            Ok(NodeResult::Update(s + 1))
        })
        .add_node("c", |s, _c: NodeContext| async move {
            Ok(NodeResult::Update(s + 1))
        })
        .set_entry("a")
        .add_edge("a", "b")
        .add_edge("b", "c")
        .set_finish("c")
        .compile()
        .unwrap();
    let run = graph.run(0).await.unwrap();
    assert_eq!(run.steps, 3);
    assert_eq!(run.state, 3);
}

#[tokio::test]
async fn checkpoints_persist_at_boundaries() {
    let cp = Arc::new(InMemoryCheckpointer::<i32>::new());
    let graph = GraphBuilder::<i32, i32>::overwrite()
        .add_node("a", |s, _c: NodeContext| async move {
            Ok(NodeResult::Update(s + 1))
        })
        .add_node("b", |s, _c: NodeContext| async move {
            Ok(NodeResult::Update(s + 1))
        })
        .set_entry("a")
        .add_edge("a", "b")
        .set_finish("b")
        .compile()
        .unwrap()
        .with_checkpointer(cp.clone());

    let run = graph.run_with_thread("t1", 0).await.unwrap();
    assert_eq!(run.state, 2);
    assert!(run.checkpoint_id.is_some());

    // one checkpoint per superstep boundary
    let list = cp.list("t1").await.unwrap();
    assert_eq!(list.len(), 2);
    // lineage is chained
    assert!(list[0].parent_checkpoint_id.is_none());
    assert_eq!(
        list[1].parent_checkpoint_id.as_deref(),
        Some(list[0].checkpoint_id.as_str())
    );
}

#[tokio::test]
async fn exit_durability_persists_only_terminal_checkpoint() {
    use crate::graph::checkpoint::DurabilityMode;

    let cp = Arc::new(InMemoryCheckpointer::<i32>::new());
    let graph = GraphBuilder::<i32, i32>::overwrite()
        .add_node("a", |s, _c: NodeContext| async move {
            Ok(NodeResult::Update(s + 1))
        })
        .add_node("b", |s, _c: NodeContext| async move {
            Ok(NodeResult::Update(s + 1))
        })
        .set_entry("a")
        .add_edge("a", "b")
        .set_finish("b")
        .compile()
        .unwrap()
        .with_checkpointer(cp.clone())
        .with_durability(DurabilityMode::Exit);

    let run = graph.run_with_thread("t1", 0).await.unwrap();
    assert_eq!(run.state, 2);
    // Only the terminal boundary is persisted under Exit durability.
    assert_eq!(cp.count("t1"), 1);
    assert!(run.checkpoint_id.is_some());
    let list = cp.list("t1").await.unwrap();
    assert_eq!(list.len(), 1);
    // The single record is the terminal boundary: no pending next nodes.
    assert!(list[0].next_nodes.is_empty());
}

#[tokio::test]
async fn interrupt_then_resume_reruns_node() {
    let cp = Arc::new(InMemoryCheckpointer::<i32>::new());
    let graph = GraphBuilder::<i32, i32>::overwrite()
        .add_node("approve", |s, ctx: NodeContext| async move {
            match ctx.resume {
                // resumed: apply the approved increment
                Some(value) => {
                    let bump = value.get("bump").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                    Ok(NodeResult::Update(s + bump))
                }
                // first run: pause for human approval
                None => Ok(NodeResult::Interrupt(Interrupt::new(
                    "approve",
                    json!({ "ask": "approve?" }),
                ))),
            }
        })
        .add_node("done", |s, _c: NodeContext| async move {
            Ok(NodeResult::Update(s))
        })
        .set_entry("approve")
        .add_edge("approve", "done")
        .set_finish("done")
        .compile()
        .unwrap()
        .with_checkpointer(cp.clone());

    // first run pauses
    let paused = graph.run_with_thread("hitl", 10).await.unwrap();
    assert!(paused.is_interrupted());
    assert_eq!(paused.status.status, ExecutionStatus::Interrupted);
    assert_eq!(paused.interrupts.len(), 1);

    // resume re-runs the interrupted node with the resume value
    let resumed = graph
        .resume("hitl", Command::resume(json!({ "bump": 5 })))
        .await
        .unwrap();
    assert!(!resumed.is_interrupted());
    assert_eq!(resumed.state, 15);
    assert_eq!(resumed.status.status, ExecutionStatus::Completed);
}

#[tokio::test]
async fn resume_emits_restore_not_save_for_the_loaded_checkpoint() {
    // Resuming loads a checkpoint; that read must surface as CheckpointRestored,
    // never CheckpointSaved (which would inflate persisted-checkpoint counts).
    let cp = Arc::new(InMemoryCheckpointer::<i32>::new());
    let sink = Arc::new(CollectingSink::new());
    let graph = GraphBuilder::<i32, i32>::overwrite()
        .add_node("approve", |s, ctx: NodeContext| async move {
            match ctx.resume {
                Some(_) => Ok(NodeResult::Update(s + 1)),
                None => Ok(NodeResult::Interrupt(Interrupt::new("approve", json!({})))),
            }
        })
        .set_entry("approve")
        .set_finish("approve")
        .compile()
        .unwrap()
        .with_checkpointer(cp.clone())
        .with_event_sink(sink.clone());

    let paused = graph.run_with_thread("t", 0).await.unwrap();
    let loaded = paused
        .checkpoint_id
        .clone()
        .expect("interrupt persisted a checkpoint");

    // Only inspect events emitted during the resume (the initial run genuinely
    // saved the interrupt checkpoint).
    let before = sink.events().len();
    graph
        .resume("t", Command::resume(json!(null)))
        .await
        .unwrap();
    let resume_events = sink.events();
    let resume_events = &resume_events[before..];

    assert!(
        resume_events.iter().any(|e| matches!(
            e,
            GraphEvent::CheckpointRestored { checkpoint_id } if *checkpoint_id == loaded
        )),
        "resume must emit CheckpointRestored for the loaded checkpoint"
    );
    assert!(
        !resume_events.iter().any(|e| matches!(
            e,
            GraphEvent::CheckpointSaved { checkpoint_id } if *checkpoint_id == loaded
        )),
        "loading a checkpoint on resume must not re-emit it as saved"
    );
}

#[tokio::test]
async fn resume_preserves_parent_checkpoint_lineage() {
    // A run that boundary-checkpoints, interrupts, then resumes to completion
    // must keep a single connected lineage: the first post-resume checkpoint
    // chains onto the loaded one instead of orphaning the pre-interrupt
    // history. Without it, get_state_history stops at the resume point and
    // prune deletes the ancestors it should protect.
    let cp = Arc::new(InMemoryCheckpointer::<i32>::new());
    let graph = GraphBuilder::<i32, i32>::overwrite()
        .add_node("start", |s, _c: NodeContext| async move {
            Ok(NodeResult::Update(s))
        })
        .add_node("approve", |s, ctx: NodeContext| async move {
            match ctx.resume {
                Some(_) => Ok(NodeResult::Update(s + 1)),
                None => Ok(NodeResult::Interrupt(Interrupt::new("approve", json!({})))),
            }
        })
        .add_node("done", |s, _c: NodeContext| async move {
            Ok(NodeResult::Update(s))
        })
        .set_entry("start")
        .add_edge("start", "approve")
        .add_edge("approve", "done")
        .set_finish("done")
        .compile()
        .unwrap()
        .with_checkpointer(cp.clone());

    let paused = graph.run_with_thread("hitl", 10).await.unwrap();
    assert!(paused.is_interrupted());
    let resumed = graph
        .resume("hitl", Command::resume(json!(null)))
        .await
        .unwrap();
    assert!(!resumed.is_interrupted());

    // Four boundary checkpoints: start, approve(interrupt), approve(resumed),
    // done — all reachable through the parent lineage from the latest.
    let history = graph.get_state_history("hitl", None).await.unwrap();
    assert_eq!(
        history.len(),
        4,
        "full lineage must walk past the resume point; got steps {:?}",
        history.iter().map(|s| s.metadata.step).collect::<Vec<_>>()
    );
    // Connected chain: exactly one root, every parent present.
    let ids: std::collections::HashSet<&str> = history
        .iter()
        .map(|s| s.metadata.checkpoint_id.as_str())
        .collect();
    let roots = history
        .iter()
        .filter(|s| s.metadata.parent_checkpoint_id.is_none())
        .count();
    assert_eq!(roots, 1, "a connected lineage has exactly one root");
    for s in &history {
        if let Some(parent) = &s.metadata.parent_checkpoint_id {
            assert!(
                ids.contains(parent.as_str()),
                "parent `{parent}` must be present in the walked history"
            );
        }
    }

    // Prune protects the ancestor chain of the retained window: keeping the
    // latest still keeps the pre-interrupt checkpoints it depends on.
    cp.prune("hitl", 1).await.unwrap();
    assert_eq!(
        cp.list("hitl").await.unwrap().len(),
        4,
        "prune must retain the full ancestor chain across the resume boundary"
    );
}

#[tokio::test]
async fn interrupt_without_checkpointer_errors_instead_of_pausing() {
    let graph = GraphBuilder::<i32, i32>::overwrite()
        .add_node("approve", |_s, _ctx: NodeContext| async move {
            Ok(NodeResult::Interrupt(Interrupt::new(
                "approve",
                json!({ "ask": "approve?" }),
            )))
        })
        .set_entry("approve")
        .set_finish("approve")
        .compile()
        .unwrap();

    let err = graph.run_with_thread("hitl", 10).await.unwrap_err();
    assert!(matches!(err, GraphError::Resume(_)), "got {err:?}");
    assert!(err.to_string().contains("checkpointer"), "{err}");
}

#[tokio::test]
async fn interrupt_without_thread_errors_instead_of_pausing() {
    let graph = GraphBuilder::<i32, i32>::overwrite()
        .add_node("approve", |_s, _ctx: NodeContext| async move {
            Ok(NodeResult::Interrupt(Interrupt::new(
                "approve",
                json!({ "ask": "approve?" }),
            )))
        })
        .set_entry("approve")
        .set_finish("approve")
        .compile()
        .unwrap()
        .with_checkpointer(Arc::new(InMemoryCheckpointer::<i32>::new()));

    let err = graph.run(10).await.unwrap_err();
    assert!(matches!(err, GraphError::Resume(_)), "got {err:?}");
    assert!(err.to_string().contains("thread id"), "{err}");
}
