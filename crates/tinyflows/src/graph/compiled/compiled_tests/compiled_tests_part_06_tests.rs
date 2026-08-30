
#[tokio::test]
async fn parallel_partial_progress_is_preserved_on_failure() {
    // A parallel step where one branch succeeds and a lower/higher-index branch
    // fails: the successful branch's update is folded into committed state and
    // the failure checkpoint schedules only the failed branch for re-run.
    let cp = Arc::new(InMemoryCheckpointer::<i32>::new());
    let attempts = Arc::new(AtomicUsize::new(0));
    let graph = GraphBuilder::<i32, i32>::new()
        .with_parallel(true)
        .set_reducer(ClosureStateReducer::new(|s: i32, u: i32| Ok(s + u)))
        .add_node("seed", |_s, _c: NodeContext| async move {
            Ok(NodeResult::Command(Command::goto(["ok", "flaky"])))
        })
        .add_node("ok", |_s, _c: NodeContext| async move {
            Ok(NodeResult::Update(1))
        })
        .add_node("flaky", move |_s, _c: NodeContext| {
            let attempts = attempts.clone();
            async move {
                // Fail on the first invocation, succeed on resume.
                if attempts.fetch_add(1, AtomicOrdering::SeqCst) == 0 {
                    Err(GraphError::Graph("branch blip".into()))
                } else {
                    Ok(NodeResult::Update(10))
                }
            }
        })
        .set_entry("seed")
        .mark_command_routing("seed")
        .set_finish("ok")
        .set_finish("flaky")
        .compile()
        .unwrap()
        .with_checkpointer(cp.clone());

    // First run: seed fans out; the "ok" branch commits +1, "flaky" aborts.
    graph.run_with_thread("fanout", 0).await.unwrap_err();
    let snapshot = graph.get_state("fanout", None).await.unwrap().unwrap();
    assert_eq!(
        snapshot.values, 1,
        "the successful branch's +1 is preserved"
    );
    assert!(
        snapshot.next_nodes.contains(&NodeId::from("flaky")),
        "only the failed branch is scheduled for re-run: {:?}",
        snapshot.next_nodes
    );

    // Resume: the flaky branch now succeeds (+10) without re-running "ok".
    let resumed = graph.retry("fanout").await.unwrap();
    assert_eq!(resumed.state, 11, "1 (preserved) + 10 (re-run branch)");
    assert_eq!(resumed.status.status, ExecutionStatus::Completed);
}

// ---------------------------------------------------------------------------
// DurabilityMode::Async
// ---------------------------------------------------------------------------

/// Delegating checkpointer whose `put` sleeps first, then records completion,
/// so tests can observe whether the executor awaited the write inline.
struct SlowCheckpointer {
    inner: Arc<InMemoryCheckpointer<i32>>,
    delay: Duration,
    completed_puts: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Checkpointer<i32> for SlowCheckpointer {
    async fn put(
        &self,
        checkpoint: crate::graph::checkpoint::Checkpoint<i32>,
    ) -> crate::graph::error::Result<crate::graph::ids::CheckpointId> {
        tokio::time::sleep(self.delay).await;
        let id = self.inner.put(checkpoint).await?;
        self.completed_puts.fetch_add(1, AtomicOrdering::SeqCst);
        Ok(id)
    }

    async fn get(
        &self,
        thread_id: &str,
        checkpoint_id: Option<&str>,
    ) -> crate::graph::error::Result<Option<crate::graph::checkpoint::Checkpoint<i32>>> {
        self.inner.get(thread_id, checkpoint_id).await
    }

    async fn list(
        &self,
        thread_id: &str,
    ) -> crate::graph::error::Result<Vec<crate::graph::checkpoint::CheckpointMetadata>> {
        self.inner.list(thread_id).await
    }

    async fn list_threads(&self) -> crate::graph::error::Result<Vec<String>> {
        self.inner.list_threads().await
    }

    async fn delete_thread(&self, thread_id: &str) -> crate::graph::error::Result<()> {
        self.inner.delete_thread(thread_id).await
    }

    async fn delete_checkpoints(
        &self,
        thread_id: &str,
        ids: &[String],
    ) -> crate::graph::error::Result<usize> {
        self.inner.delete_checkpoints(thread_id, ids).await
    }
}

/// Delegating checkpointer that fails every *non-terminal* boundary `put`
/// (records with pending next nodes), simulating a broken store while the
/// terminal write still succeeds.
struct FailNonTerminalCheckpointer {
    inner: Arc<InMemoryCheckpointer<i32>>,
}

#[async_trait::async_trait]
impl Checkpointer<i32> for FailNonTerminalCheckpointer {
    async fn put(
        &self,
        checkpoint: crate::graph::checkpoint::Checkpoint<i32>,
    ) -> crate::graph::error::Result<crate::graph::ids::CheckpointId> {
        if !checkpoint.next_nodes.is_empty() {
            return Err(crate::graph::error::GraphError::Checkpoint(
                "injected background write failure".to_string(),
            ));
        }
        self.inner.put(checkpoint).await
    }

    async fn get(
        &self,
        thread_id: &str,
        checkpoint_id: Option<&str>,
    ) -> crate::graph::error::Result<Option<crate::graph::checkpoint::Checkpoint<i32>>> {
        self.inner.get(thread_id, checkpoint_id).await
    }

    async fn list(
        &self,
        thread_id: &str,
    ) -> crate::graph::error::Result<Vec<crate::graph::checkpoint::CheckpointMetadata>> {
        self.inner.list(thread_id).await
    }

    async fn list_threads(&self) -> crate::graph::error::Result<Vec<String>> {
        self.inner.list_threads().await
    }

    async fn delete_thread(&self, thread_id: &str) -> crate::graph::error::Result<()> {
        self.inner.delete_thread(thread_id).await
    }

    async fn delete_checkpoints(
        &self,
        thread_id: &str,
        ids: &[String],
    ) -> crate::graph::error::Result<usize> {
        self.inner.delete_checkpoints(thread_id, ids).await
    }
}

fn two_step_graph() -> crate::graph::GraphBuilder<i32, i32> {
    GraphBuilder::<i32, i32>::overwrite()
        .add_node("a", |s, _c: NodeContext| async move {
            Ok(NodeResult::Update(s + 1))
        })
        .add_node("b", |s, _c: NodeContext| async move {
            Ok(NodeResult::Update(s + 1))
        })
        .set_entry("a")
        .add_edge("a", "b")
        .set_finish("b")
}

#[tokio::test]
async fn async_durability_persists_every_boundary_with_intact_lineage() {
    use crate::graph::checkpoint::DurabilityMode;

    let cp = Arc::new(InMemoryCheckpointer::<i32>::new());
    let graph = two_step_graph()
        .compile()
        .unwrap()
        .with_checkpointer(cp.clone())
        .with_durability(DurabilityMode::Async);

    let run = graph.run_with_thread("t-async", 0).await.unwrap();
    assert_eq!(run.state, 2);
    // Both boundaries are durable by run end: the run drained its background
    // writes before writing the terminal checkpoint.
    let list = cp.list("t-async").await.unwrap();
    assert_eq!(list.len(), 2);
    // Lineage stays chained even though the first write ran in the background
    // (its id was minted before the write was handed off).
    assert_eq!(
        list[1].parent_checkpoint_id.as_deref(),
        Some(list[0].checkpoint_id.as_str())
    );
    assert_eq!(
        run.checkpoint_id.as_ref().map(|id| id.as_str()),
        Some(list[1].checkpoint_id.as_str())
    );
}

#[tokio::test]
async fn async_durability_does_not_await_non_terminal_writes_inline() {
    use crate::graph::checkpoint::DurabilityMode;

    let completed_puts = Arc::new(AtomicUsize::new(0));
    let observed_at_b = Arc::new(AtomicUsize::new(usize::MAX));
    let cp = Arc::new(SlowCheckpointer {
        inner: Arc::new(InMemoryCheckpointer::new()),
        delay: Duration::from_millis(100),
        completed_puts: completed_puts.clone(),
    });

    let puts_for_b = completed_puts.clone();
    let seen = observed_at_b.clone();
    let graph = GraphBuilder::<i32, i32>::overwrite()
        .add_node("a", |s, _c: NodeContext| async move {
            Ok(NodeResult::Update(s + 1))
        })
        .add_node("b", move |s, _c: NodeContext| {
            let puts = puts_for_b.clone();
            let seen = seen.clone();
            async move {
                // Record how many checkpoint writes had *completed* when this
                // node ran. Under Sync durability the step-1 boundary write
                // (100ms) would have finished first; under Async it is still
                // in flight.
                seen.store(puts.load(AtomicOrdering::SeqCst), AtomicOrdering::SeqCst);
                Ok(NodeResult::Update(s + 1))
            }
        })
        .set_entry("a")
        .add_edge("a", "b")
        .set_finish("b")
        .compile()
        .unwrap()
        .with_checkpointer(cp)
        .with_durability(DurabilityMode::Async);

    let run = graph.run_with_thread("t-async-slow", 0).await.unwrap();
    assert_eq!(run.state, 2);
    assert_eq!(
        observed_at_b.load(AtomicOrdering::SeqCst),
        0,
        "node b must start while the step-1 boundary write is still in flight"
    );
    // ...but by run end every write has been drained and is durable.
    assert_eq!(completed_puts.load(AtomicOrdering::SeqCst), 2);
}

#[tokio::test]
async fn async_durability_surfaces_background_write_failure_in_run_result() {
    use crate::graph::checkpoint::DurabilityMode;

    let cp = Arc::new(FailNonTerminalCheckpointer {
        inner: Arc::new(InMemoryCheckpointer::new()),
    });
    let graph = two_step_graph()
        .compile()
        .unwrap()
        .with_checkpointer(cp)
        .with_durability(DurabilityMode::Async);

    // The step-1 boundary write fails in the background; the run must not
    // report success — the failure surfaces at the next durability boundary
    // or, at the latest, at the terminal drain.
    let err = graph
        .run_with_thread("t-async-fail", 0)
        .await
        .expect_err("a lost background checkpoint must fail the run");
    assert!(
        err.to_string()
            .contains("injected background write failure"),
        "unexpected error: {err}"
    );
}

// ---------------------------------------------------------------------------
// Regression: resume value targeting, barrier-gated manual writes, async drains
// ---------------------------------------------------------------------------

#[tokio::test]
async fn resume_value_reaches_only_the_interrupted_node() {
    // Parallel [a, b]: a routes to successor `x` and completes; b interrupts.
    // The interrupt boundary schedules both `x` and `b`, but only `b` actually
    // interrupted — `x` has never run, so it must observe `ctx.resume == None`
    // on its first activation. Fanning the resume value across the whole
    // pending set used to drive `x` down its "already approved" arm.
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
        // 10 on a first (unresumed) activation, 1000 if it wrongly sees a
        // resume value it never asked for.
        .add_node("x", |_s: Counter, c: NodeContext| async move {
            Ok(NodeResult::Update(if c.resume.is_some() {
                1000
            } else {
                10
            }))
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
            "t-resume-scope",
            Counter {
                value: 0,
                log: vec![],
            },
        )
        .await
        .unwrap();
    assert!(paused.is_interrupted());

    let done = graph
        .resume("t-resume-scope", Command::resume(json!({"approved": true})))
        .await
        .unwrap();
    // 1 (a) + 100 (b, resumed) + 10 (x, first activation, no resume value).
    assert_eq!(
        done.state.value, 111,
        "the resume value must reach only the node that interrupted"
    );
}
