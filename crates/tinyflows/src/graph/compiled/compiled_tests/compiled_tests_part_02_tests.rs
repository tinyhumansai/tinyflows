
#[tokio::test]
async fn resume_without_checkpointer_errors() {
    let graph = GraphBuilder::<i32, i32>::overwrite()
        .add_node("a", |s, _c: NodeContext| async move {
            Ok(NodeResult::Update(s))
        })
        .set_entry("a")
        .set_finish("a")
        .compile()
        .unwrap();
    let err = graph
        .resume("t", Command::resume(json!(null)))
        .await
        .unwrap_err();
    assert!(matches!(err, GraphError::Resume(_)));
}

#[tokio::test]
async fn events_are_emitted() {
    let sink = Arc::new(CollectingSink::new());
    let graph = GraphBuilder::<i32, i32>::overwrite()
        .add_node("a", |s, _c: NodeContext| async move {
            Ok(NodeResult::Update(s + 1))
        })
        .set_entry("a")
        .set_finish("a")
        .compile()
        .unwrap()
        .with_event_sink(sink.clone());

    graph.run(1).await.unwrap();
    let events = sink.events();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, GraphEvent::StepStarted { .. }))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, GraphEvent::NodeCompleted { .. }))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, GraphEvent::StepCompleted { .. }))
    );
}

// --- State inspection & time travel ----------------------------------------

/// A linear `a -> b -> c` counter graph (each node `+1`) wired to `cp`, used by
/// the inspection/time-travel tests.
fn chain_graph(cp: Arc<InMemoryCheckpointer<i32>>) -> CompiledGraph<i32, i32> {
    GraphBuilder::<i32, i32>::overwrite()
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
        .unwrap()
        .with_checkpointer(cp)
}

#[tokio::test]
async fn get_state_and_history_walk_the_lineage() {
    use crate::graph::CheckpointSource;

    let cp = Arc::new(InMemoryCheckpointer::<i32>::new());
    let graph = chain_graph(cp.clone());
    graph.run_with_thread("t", 0).await.unwrap();

    // Latest snapshot is the terminal boundary: state 3, no pending nodes.
    let latest = graph.get_state("t", None).await.unwrap().unwrap();
    assert_eq!(latest.values, 3);
    assert!(latest.next_nodes.is_empty());
    assert_eq!(latest.metadata.source, CheckpointSource::Loop);

    // History is newest-first along the parent chain: 3 boundaries.
    let history = graph.get_state_history("t", None).await.unwrap();
    assert_eq!(
        history.iter().map(|s| s.values).collect::<Vec<_>>(),
        vec![3, 2, 1]
    );
    // The oldest snapshot has no parent; younger ones chain to their parent.
    assert!(history.last().unwrap().parent_config.is_none());
    assert_eq!(
        history[0].parent_config.as_ref().unwrap().checkpoint_id,
        history[1].config.checkpoint_id,
    );

    // limit caps to the most recent snapshots.
    let limited = graph.get_state_history("t", Some(2)).await.unwrap();
    assert_eq!(limited.len(), 2);
    assert_eq!(limited[0].values, 3);

    // Unknown thread / missing checkpointer behave as documented.
    assert!(graph.get_state("missing", None).await.unwrap().is_none());
}

#[tokio::test]
async fn update_state_goes_through_the_reducer() {
    use crate::graph::CheckpointSource;

    let cp = Arc::new(InMemoryCheckpointer::<Counter>::new());
    let graph = adding_graph().with_checkpointer(cp.clone());
    graph
        .run_with_thread(
            "t",
            Counter {
                value: 0,
                log: vec![],
            },
        )
        .await
        .unwrap();

    // Manual write: the reducer adds 10 and records a log entry (proving it is
    // not a raw overwrite).
    let config = graph.update_state("t", 10, None).await.unwrap();
    let snap = graph
        .get_state("t", Some(&config.checkpoint_id.unwrap()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snap.values.value, 12);
    assert_eq!(snap.values.log, vec!["+1", "+1", "+10"]);
    assert_eq!(snap.metadata.source, CheckpointSource::Update);

    // Attributing to a missing node is rejected.
    let err = graph
        .update_state("t", 1, Some("nope".into()))
        .await
        .unwrap_err();
    assert!(matches!(err, GraphError::MissingNode(_)));
}

#[tokio::test]
async fn update_state_as_node_sets_successor_pending_nodes() {
    let cp = Arc::new(InMemoryCheckpointer::<i32>::new());
    let graph = chain_graph(cp.clone());
    graph.run_with_thread("t", 0).await.unwrap();

    // Attribute a write to `a`: the new checkpoint's pending nodes become a's
    // successor (`b`), so a resume continues from there.
    let config = graph.update_state("t", 5, Some("a".into())).await.unwrap();
    let snap = graph
        .get_state("t", Some(&config.checkpoint_id.unwrap()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        snap.next_nodes
            .iter()
            .map(|n| n.to_string())
            .collect::<Vec<_>>(),
        vec!["b".to_string()]
    );
}

#[tokio::test]
async fn update_state_as_command_node_is_rejected() {
    // A command node routes dynamically, so it has no static successors. Using
    // it as `as_node` would persist an empty `next_nodes` and silently render
    // the thread non-resumable; the write must be rejected instead.
    let cp = Arc::new(InMemoryCheckpointer::<i32>::new());
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
        .unwrap()
        .with_checkpointer(cp);
    graph.run_with_thread("t", 0).await.unwrap();

    let err = graph
        .update_state("t", 1, Some("router".into()))
        .await
        .unwrap_err();
    assert!(matches!(err, GraphError::Graph(_)), "got {err:?}");
    assert!(err.to_string().contains("non-resumable"), "{err}");

    // A plain node is still accepted.
    graph
        .update_state("t", 1, Some("target".into()))
        .await
        .unwrap();
}

#[tokio::test]
async fn bulk_update_state_applies_successive_updates() {
    use crate::graph::CheckpointSource;

    let cp = Arc::new(InMemoryCheckpointer::<i32>::new());
    let graph = chain_graph(cp.clone());
    graph.run_with_thread("t", 0).await.unwrap();
    let before = cp.count("t");

    let last = graph
        .bulk_update_state("t", [(10, None), (100, None)])
        .await
        .unwrap();
    // Two new update checkpoints were appended.
    assert_eq!(cp.count("t"), before + 2);
    let snap = graph
        .get_state("t", Some(&last.checkpoint_id.unwrap()))
        .await
        .unwrap()
        .unwrap();
    // overwrite reducer: 3 -> 10 -> 100 (last write wins each step).
    assert_eq!(snap.values, 100);
    assert_eq!(snap.metadata.source, CheckpointSource::Update);

    // Empty bulk is rejected (no resulting config).
    let err = graph.bulk_update_state("t", []).await.unwrap_err();
    assert!(matches!(err, GraphError::Checkpoint(_)));
}

#[tokio::test]
async fn fork_state_does_not_mutate_source() {
    use crate::graph::CheckpointSource;

    let cp = Arc::new(InMemoryCheckpointer::<i32>::new());
    let graph = chain_graph(cp.clone());
    graph.run_with_thread("src", 0).await.unwrap();
    let src_before = cp.count("src");
    let src_latest = graph.get_state("src", None).await.unwrap().unwrap();

    let forked = graph.fork_state("src", None, "dst").await.unwrap();
    // Source thread is untouched: same count, same latest state/source.
    assert_eq!(cp.count("src"), src_before);
    let src_after = graph.get_state("src", None).await.unwrap().unwrap();
    assert_eq!(src_after.values, src_latest.values);
    assert_eq!(src_after.metadata.source, src_latest.metadata.source);

    // Target carries the forked state as a fresh root (no parent), source=fork.
    let dst = graph
        .get_state("dst", Some(&forked.checkpoint_id.unwrap()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(dst.values, src_latest.values);
    assert_eq!(dst.metadata.source, CheckpointSource::Fork);
    assert!(dst.parent_config.is_none());
}

#[tokio::test]
async fn resume_from_older_checkpoint_replays_forward() {
    let cp = Arc::new(InMemoryCheckpointer::<i32>::new());
    let graph = chain_graph(cp.clone());
    graph.run_with_thread("t", 0).await.unwrap();

    // The first boundary (after `a`) has state 1 and pending node `b`.
    let list = cp.list("t").await.unwrap();
    let after_a = &list[0];
    assert_eq!(
        after_a
            .next_nodes
            .iter()
            .map(|n| n.to_string())
            .collect::<Vec<_>>(),
        vec!["b".to_string()]
    );

    // Time-travel resume from that older checkpoint replays b -> c forward.
    let replayed = graph
        .resume_from(
            "t",
            ResumeTarget::Checkpoint(after_a.checkpoint_id.clone()),
            Command::new(),
        )
        .await
        .unwrap();
    assert!(!replayed.is_interrupted());
    assert_eq!(replayed.state, 3);

    // Resuming an unknown checkpoint id errors.
    let err = graph
        .resume_from("t", ResumeTarget::Checkpoint("nope".into()), Command::new())
        .await
        .unwrap_err();
    assert!(matches!(err, GraphError::Resume(_)));
}

// --- Parallel (fan-out / fan-in) execution ---------------------------------

#[derive(Clone, Debug, Default, PartialEq)]
struct Fan {
    /// Values contributed by branches, in reducer-application order.
    values: Vec<i32>,
    /// Fork branch indices observed by branches, in reducer-application order.
    forks: Vec<usize>,
    /// Sum a downstream join node computed over the merged `values`.
    joined_sum: Option<i32>,
}

#[derive(Clone, Debug)]
enum FanUpdate {
    Branch { value: i32, fork: usize },
    Join(i32),
}

/// Shared instrumentation proving how many branches were in flight at once.
#[derive(Clone)]
struct Concurrency {
    inflight: Arc<AtomicUsize>,
    max: Arc<AtomicUsize>,
}

impl Concurrency {
    fn new() -> Self {
        Self {
            inflight: Arc::new(AtomicUsize::new(0)),
            max: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn max_observed(&self) -> usize {
        self.max.load(AtomicOrdering::SeqCst)
    }

    async fn track<T>(&self, sleep: Duration, value: T) -> T {
        let now = self.inflight.fetch_add(1, AtomicOrdering::SeqCst) + 1;
        self.max.fetch_max(now, AtomicOrdering::SeqCst);
        tokio::time::sleep(sleep).await;
        self.inflight.fetch_sub(1, AtomicOrdering::SeqCst);
        value
    }
}

/// Builds a fan-out/fan-in graph: `super` routes to three branches that each
/// contribute a value and their fork index; all three converge on `join`, which
/// observes the merged state. `parallel` toggles concurrent branch execution.
/// Branch sleeps are deliberately reversed (a shortest, c longest) so reducer
/// ordering cannot accidentally match completion order.
fn fanout_graph(parallel: bool, conc: Concurrency) -> CompiledGraph<Fan, FanUpdate> {
    let (c_a, c_b, c_c) = (conc.clone(), conc.clone(), conc);
    GraphBuilder::<Fan, FanUpdate>::new()
        .with_parallel(parallel)
        .set_reducer(ClosureStateReducer::new(|mut s: Fan, u: FanUpdate| {
            match u {
                FanUpdate::Branch { value, fork } => {
                    s.values.push(value);
                    s.forks.push(fork);
                }
                FanUpdate::Join(sum) => s.joined_sum = Some(sum),
            }
            Ok(s)
        }))
        .add_node("super", |_s: Fan, _c: NodeContext| async move {
            Ok(NodeResult::Command(
                Command::default().with_goto(["a", "b", "c"]),
            ))
        })
        .add_node("a", move |_s: Fan, c: NodeContext| {
            let conc = c_a.clone();
            let fork = c
                .fork
                .as_ref()
                .map(|f| f.branch_index)
                .unwrap_or(usize::MAX);
            async move {
                Ok(NodeResult::Update(
                    conc.track(
                        Duration::from_millis(20),
                        FanUpdate::Branch { value: 1, fork },
                    )
                    .await,
                ))
            }
        })
        .add_node("b", move |_s: Fan, c: NodeContext| {
            let conc = c_b.clone();
            let fork = c
                .fork
                .as_ref()
                .map(|f| f.branch_index)
                .unwrap_or(usize::MAX);
            async move {
                Ok(NodeResult::Update(
                    conc.track(
                        Duration::from_millis(60),
                        FanUpdate::Branch { value: 2, fork },
                    )
                    .await,
                ))
            }
        })
        .add_node("c", move |_s: Fan, c: NodeContext| {
            let conc = c_c.clone();
            let fork = c
                .fork
                .as_ref()
                .map(|f| f.branch_index)
                .unwrap_or(usize::MAX);
            async move {
                Ok(NodeResult::Update(
                    conc.track(
                        Duration::from_millis(100),
                        FanUpdate::Branch { value: 4, fork },
                    )
                    .await,
                ))
            }
        })
        .add_node("join", |s: Fan, _c: NodeContext| async move {
            Ok(NodeResult::Update(FanUpdate::Join(s.values.iter().sum())))
        })
        .set_entry("super")
        .mark_command_routing("super")
        .add_edge("a", "join")
        .add_edge("b", "join")
        .add_edge("c", "join")
        .set_finish("join")
        .compile()
        .unwrap()
}
