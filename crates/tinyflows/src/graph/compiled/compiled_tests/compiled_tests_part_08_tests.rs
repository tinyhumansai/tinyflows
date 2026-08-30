
#[tokio::test]
async fn async_durability_drains_background_writes_on_abort() {
    // The recursion-limit abort returns `Err` mid-run. Any in-flight background
    // checkpoint write must be settled first: dropping the tracker would detach
    // the tasks, discarding their outcome and racing a caller that immediately
    // retries the thread.
    use crate::graph::checkpoint::DurabilityMode;

    let completed_puts = Arc::new(AtomicUsize::new(0));
    let cp = Arc::new(SlowCheckpointer {
        inner: Arc::new(InMemoryCheckpointer::new()),
        delay: Duration::from_millis(50),
        completed_puts: completed_puts.clone(),
    });
    let graph = GraphBuilder::<i32, i32>::overwrite()
        .add_node("a", |s, _c: NodeContext| async move {
            Ok(NodeResult::Update(s + 1))
        })
        .set_entry("a")
        .add_edge("a", "a")
        .with_recursion_limit(3)
        .compile()
        .unwrap()
        .with_checkpointer(cp)
        .with_durability(DurabilityMode::Async);

    let err = graph
        .run_with_thread("t-async-abort", 0)
        .await
        .expect_err("the run must abort at the recursion limit");
    assert!(matches!(err, GraphError::RecursionLimit(3)));
    assert_eq!(
        completed_puts.load(AtomicOrdering::SeqCst),
        3,
        "every background boundary write must be settled before the abort returns"
    );
}

/// Delegating checkpointer that makes the *first* `put` slow and every later
/// one instant, so an unserialized background writer would append boundary 2
/// before boundary 1.
struct FirstPutSlowCheckpointer {
    inner: Arc<InMemoryCheckpointer<i32>>,
    started: AtomicUsize,
}

#[async_trait::async_trait]
impl Checkpointer<i32> for FirstPutSlowCheckpointer {
    async fn put(
        &self,
        checkpoint: crate::graph::checkpoint::Checkpoint<i32>,
    ) -> crate::graph::error::Result<crate::graph::ids::CheckpointId> {
        if self.started.fetch_add(1, AtomicOrdering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(100)).await;
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

#[tokio::test]
async fn async_durability_appends_boundaries_in_order() {
    // Every bundled backend defines a thread's "latest" checkpoint by insertion
    // order, so background writes must land in boundary order even when an
    // earlier `put` is slower than a later one.
    use crate::graph::checkpoint::DurabilityMode;

    let cp = Arc::new(FirstPutSlowCheckpointer {
        inner: Arc::new(InMemoryCheckpointer::new()),
        started: AtomicUsize::new(0),
    });
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
        .unwrap()
        .with_checkpointer(cp.clone())
        .with_durability(DurabilityMode::Async);

    let run = graph.run_with_thread("t-async-order", 0).await.unwrap();
    assert_eq!(run.state, 3);
    // Insertion order is listing order: each record's parent must be the one
    // appended just before it.
    let list = cp.list("t-async-order").await.unwrap();
    assert_eq!(list.len(), 3);
    for pair in list.windows(2) {
        assert_eq!(
            pair[1].parent_checkpoint_id.as_deref(),
            Some(pair[0].checkpoint_id.as_str()),
            "boundary writes landed out of order"
        );
    }
}

#[tokio::test]
async fn legacy_interrupt_checkpoint_without_stamped_nodes_still_resumes() {
    // Checkpoints written before the `interrupted_nodes` metadata existed carry
    // only `Interrupt::node`, which for a re-emitted child interrupt (what a
    // subgraph node does) names a node this graph never schedules. The resume
    // value must still reach the paused node — falling back to the pending set —
    // rather than being addressed to a node that does not exist here, which
    // would re-run the paused node unresumed and interrupt forever.
    let cp = Arc::new(InMemoryCheckpointer::<Counter>::new());
    let graph = GraphBuilder::<Counter, i32>::new()
        .set_reducer(ClosureStateReducer::new(|mut s: Counter, u: i32| {
            s.value += u;
            s.log.push(format!("+{u}"));
            Ok(s)
        }))
        .add_node("gate", |_s: Counter, c: NodeContext| async move {
            match c.resume {
                // The interrupt names a foreign node, as a subgraph node's
                // re-emitted child interrupt does.
                None => Ok(NodeResult::Interrupt(Interrupt::new(
                    "child-gate",
                    json!({}),
                ))),
                Some(_) => Ok(NodeResult::Update(5)),
            }
        })
        .set_entry("gate")
        .set_finish("gate")
        .compile()
        .unwrap()
        .with_checkpointer(cp.clone());

    let paused = graph
        .run_with_thread(
            "t-legacy-resume",
            Counter {
                value: 0,
                log: vec![],
            },
        )
        .await
        .unwrap();
    assert!(paused.is_interrupted());

    // Age the boundary checkpoint into its pre-upgrade shape.
    let mut legacy = cp.get("t-legacy-resume", None).await.unwrap().unwrap();
    legacy
        .metadata
        .as_object_mut()
        .unwrap()
        .remove("interrupted_nodes");
    cp.put(legacy).await.unwrap();

    let done = graph
        .resume("t-legacy-resume", Command::resume(json!("go")))
        .await
        .unwrap();
    assert!(
        !done.is_interrupted(),
        "a legacy interrupt checkpoint must still deliver the resume value"
    );
    assert_eq!(done.state.value, 5);
}

/// Delegating checkpointer whose first `put` fails *slowly* (so the next
/// boundary's write is already chained behind it) and records every attempt.
struct FirstPutFailsSlowlyCheckpointer {
    inner: Arc<InMemoryCheckpointer<i32>>,
    attempts: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Checkpointer<i32> for FirstPutFailsSlowlyCheckpointer {
    async fn put(
        &self,
        checkpoint: crate::graph::checkpoint::Checkpoint<i32>,
    ) -> crate::graph::error::Result<crate::graph::ids::CheckpointId> {
        if self.attempts.fetch_add(1, AtomicOrdering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(100)).await;
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

#[tokio::test]
async fn async_durability_skips_a_write_whose_predecessor_failed() {
    // Writing boundary N+1 after boundary N failed would durably append a
    // record whose `parent_checkpoint_id` points at something that never
    // persisted. The chained write must skip its own `put` and report the
    // failure that broke the lineage.
    use crate::graph::checkpoint::DurabilityMode;

    let attempts = Arc::new(AtomicUsize::new(0));
    let cp = Arc::new(FirstPutFailsSlowlyCheckpointer {
        inner: Arc::new(InMemoryCheckpointer::new()),
        attempts: attempts.clone(),
    });
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
        .unwrap()
        .with_checkpointer(cp.clone())
        .with_durability(DurabilityMode::Async);

    let err = graph
        .run_with_thread("t-async-orphan", 0)
        .await
        .expect_err("a lost background checkpoint must fail the run");
    assert!(
        err.to_string()
            .contains("injected background write failure"),
        "unexpected error: {err}"
    );
    assert_eq!(
        attempts.load(AtomicOrdering::SeqCst),
        1,
        "the write chained behind a failed one must not be attempted"
    );
    assert!(
        cp.list("t-async-orphan").await.unwrap().is_empty(),
        "no orphaned checkpoint may be appended after a broken lineage"
    );
}

/// A per-node concurrency cap bounds how many activations of that node run at
/// once, without throttling the rest of the step.
///
/// Six `Send` packets fan `worker` out six ways alongside an unrelated `other`
/// branch. With `worker` capped at 2, at most two workers may ever be in flight
/// — but `other` must not be made to wait behind them, which is the whole reason
/// this is a per-node bound rather than the graph-wide one.
///
/// The assertion is on observed *overlap*, tracked by incrementing a counter on
/// entry and decrementing on exit, because a cap that silently failed to bind
/// would still produce the same final state.
#[tokio::test]
async fn a_per_node_cap_bounds_one_node_without_throttling_the_step() {
    let in_flight = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let other_ran_early = Arc::new(AtomicBool::new(false));

    let worker_in_flight = in_flight.clone();
    let worker_peak = peak.clone();
    let other_flag = other_ran_early.clone();

    let graph = GraphBuilder::<i32, i32>::new()
        .with_parallel(true)
        .with_node_concurrency("worker", 2)
        .set_reducer(ClosureStateReducer::new(|s: i32, u: i32| Ok(s + u)))
        .add_node("dispatch", |_s: i32, _c: NodeContext| async move {
            Ok(NodeResult::Command(Command::send(
                (1..=6).map(|n| Send::new("worker", json!(n))),
            )))
        })
        .add_node("worker", move |_s: i32, _c: NodeContext| {
            let in_flight = worker_in_flight.clone();
            let peak = worker_peak.clone();
            async move {
                let now = in_flight.fetch_add(1, AtomicOrdering::SeqCst) + 1;
                peak.fetch_max(now, AtomicOrdering::SeqCst);
                // Yield enough times that any un-capped sibling would overlap.
                for _ in 0..8 {
                    tokio::task::yield_now().await;
                }
                in_flight.fetch_sub(1, AtomicOrdering::SeqCst);
                Ok(NodeResult::Update(1))
            }
        })
        .add_node("other", move |_s: i32, _c: NodeContext| {
            let flag = other_flag.clone();
            async move {
                flag.store(true, AtomicOrdering::SeqCst);
                Ok(NodeResult::Update(100))
            }
        })
        .set_entry("dispatch")
        .mark_command_routing("dispatch")
        .set_finish("worker")
        .set_finish("other")
        .compile()
        .unwrap();

    // `dispatch` sends six workers; `other` is seeded alongside them so the step
    // contains both.
    let done = graph
        .run_with_inputs(
            0,
            [
                crate::graph::GraphInput::start(json!(null)),
                crate::graph::GraphInput::node("other"),
            ],
        )
        .await
        .unwrap();

    assert!(
        peak.load(AtomicOrdering::SeqCst) <= 2,
        "worker is capped at 2 concurrent activations, observed peak {}",
        peak.load(AtomicOrdering::SeqCst)
    );
    assert!(
        other_ran_early.load(AtomicOrdering::SeqCst),
        "the unrelated branch must still run — a per-node cap must not throttle \
         the whole step"
    );
    assert_eq!(
        done.state, 106,
        "all six workers plus `other` still ran: capping concurrency must not \
         drop work"
    );
}
