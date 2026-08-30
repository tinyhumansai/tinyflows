
/// `with_max_concurrency` (via `set_defaults`) bounds the number of node
/// handlers in flight at once within a parallel superstep.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn max_concurrency_bounds_in_flight_branches() {
    let in_flight = Arc::new(AtomicUsize::new(0));
    let max_seen = Arc::new(AtomicUsize::new(0));

    let worker_in_flight = in_flight.clone();
    let worker_max = max_seen.clone();

    let graph = GraphBuilder::<Counter, i32>::new()
        .set_reducer(ClosureStateReducer::new(|mut s: Counter, u: i32| {
            s.value += u;
            Ok(s)
        }))
        .set_defaults(GraphDefaults {
            parallel: Some(true),
            max_concurrency: Some(2),
            ..Default::default()
        })
        .add_node("dispatch", |_s: Counter, _c: NodeContext| async move {
            Ok(NodeResult::Command(Command::send([
                Send::new("worker", json!(1)),
                Send::new("worker", json!(1)),
                Send::new("worker", json!(1)),
                Send::new("worker", json!(1)),
            ])))
        })
        .add_node("worker", move |_s: Counter, _c: NodeContext| {
            let in_flight = worker_in_flight.clone();
            let max_seen = worker_max.clone();
            async move {
                let now = in_flight.fetch_add(1, AtomicOrdering::SeqCst) + 1;
                max_seen.fetch_max(now, AtomicOrdering::SeqCst);
                tokio::time::sleep(Duration::from_millis(30)).await;
                in_flight.fetch_sub(1, AtomicOrdering::SeqCst);
                Ok(NodeResult::Update(1))
            }
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

    // All four workers ran and contributed.
    assert_eq!(run.state.value, 4);
    // Never more than the configured bound of 2 in flight simultaneously.
    assert!(
        max_seen.load(AtomicOrdering::SeqCst) <= 2,
        "max in-flight {} exceeded bound",
        max_seen.load(AtomicOrdering::SeqCst)
    );
    // And concurrency actually happened (a chunk of 2 overlapped).
    assert_eq!(max_seen.load(AtomicOrdering::SeqCst), 2);
}

/// With `max_concurrency`, the executor uses a rolling `buffered(limit)` window
/// rather than fixed `join_all` chunks, so a slow branch does not head-of-line
/// block later branches: a new branch starts as soon as any in-flight one
/// finishes. A fixed-chunk executor would run the long branch's chunk to
/// completion before starting the next chunk, so the long branch would overlap
/// at most its single chunk-mate.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn max_concurrency_uses_rolling_window_not_chunks() {
    // A shared flag marks the long branch as running; short branches count how
    // many of them start while the long branch is still in flight.
    let long_running = Arc::new(AtomicBool::new(false));
    let overlapped_with_long = Arc::new(AtomicUsize::new(0));

    let w_long = long_running.clone();
    let w_overlap = overlapped_with_long.clone();

    let graph = GraphBuilder::<Counter, i32>::new()
        .set_reducer(ClosureStateReducer::new(|mut s: Counter, u: i32| {
            s.value += u;
            Ok(s)
        }))
        .set_defaults(GraphDefaults {
            parallel: Some(true),
            max_concurrency: Some(2),
            ..Default::default()
        })
        .add_node("dispatch", |_s: Counter, _c: NodeContext| async move {
            // One long branch (arg 100) plus three short branches (arg 5).
            Ok(NodeResult::Command(Command::send([
                Send::new("worker", json!(100)),
                Send::new("worker", json!(5)),
                Send::new("worker", json!(5)),
                Send::new("worker", json!(5)),
            ])))
        })
        .add_node("worker", move |_s: Counter, c: NodeContext| {
            let long_running = w_long.clone();
            let overlapped = w_overlap.clone();
            async move {
                let ms = c.send_arg.and_then(|v| v.as_u64()).unwrap_or(0);
                if ms >= 50 {
                    // The long branch: flag itself running for its whole life.
                    long_running.store(true, AtomicOrdering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                    long_running.store(false, AtomicOrdering::SeqCst);
                } else {
                    // A short branch: did it get to start while the long branch
                    // was still running? Only possible with a rolling window.
                    if long_running.load(AtomicOrdering::SeqCst) {
                        overlapped.fetch_add(1, AtomicOrdering::SeqCst);
                    }
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                }
                Ok(NodeResult::Update(1))
            }
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

    assert_eq!(run.state.value, 4, "all four workers ran");
    // With a rolling window the two short branches that start after the initial
    // pair (slot freed as each short one finishes) run while the long branch is
    // still going. A fixed-chunk executor would finish the long branch's chunk
    // first, so at most one short branch could overlap it.
    assert!(
        overlapped_with_long.load(AtomicOrdering::SeqCst) >= 2,
        "expected the rolling window to overlap the long branch with >=2 short \
         branches, saw {}",
        overlapped_with_long.load(AtomicOrdering::SeqCst)
    );
}

/// A per-node default timeout fails the run with [`GraphError::Timeout`]
/// when a handler does not resolve in time.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_timeout_fails_slow_handler() {
    let graph = GraphBuilder::<i32, i32>::overwrite()
        .with_node_timeout(Duration::from_millis(20))
        .add_node("slow", |s: i32, _c: NodeContext| async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            Ok(NodeResult::Update(s))
        })
        .set_entry("slow")
        .set_finish("slow")
        .compile()
        .unwrap();

    let err = graph.run(0).await.unwrap_err();
    assert!(matches!(err, GraphError::Timeout(_)));
}

// ── Whole-run wall-clock deadline ────────────────────────────────────────────

/// A per-run deadline stops the run *between* super-steps once the elapsed run
/// time reaches it, surfacing [`GraphError::Timeout`].
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_deadline_stops_between_supersteps() {
    let graph = GraphBuilder::<i32, i32>::overwrite()
        .add_node("a", |s: i32, _c: NodeContext| async move {
            tokio::time::sleep(Duration::from_millis(40)).await;
            Ok(NodeResult::Update(s + 1))
        })
        .add_node("b", |s: i32, _c: NodeContext| async move {
            Ok(NodeResult::Update(s + 1))
        })
        .set_entry("a")
        .add_edge("a", "b")
        .set_finish("b")
        .compile()
        .unwrap()
        .with_run_deadline(Duration::from_millis(20));

    // The first boundary (elapsed ~0) admits node `a`; the next boundary
    // (elapsed ~40ms ≥ 20ms) trips the deadline before `b` ever runs.
    let err = graph.run(0).await.unwrap_err();
    assert!(matches!(err, GraphError::Timeout(_)), "got {err:?}");
}

/// A run that finishes within its deadline is unaffected — no false trip.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_deadline_allows_a_run_that_finishes_in_time() {
    let graph = GraphBuilder::<i32, i32>::overwrite()
        .add_node("a", |s: i32, _c: NodeContext| async move {
            Ok(NodeResult::Update(s + 1))
        })
        .add_node("b", |s: i32, _c: NodeContext| async move {
            Ok(NodeResult::Update(s + 1))
        })
        .set_entry("a")
        .add_edge("a", "b")
        .set_finish("b")
        .compile()
        .unwrap()
        .with_run_deadline(Duration::from_secs(30));

    let run = graph.run(0).await.unwrap();
    assert_eq!(run.state, 2);
}

/// On a checkpointed thread, a deadline trip leaves the last committed boundary
/// checkpoint intact — so the run can be resumed to completion rather than lost
/// (the durability win over an external `tokio::time::timeout`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_deadline_leaves_last_checkpoint_resumable() {
    let cp = Arc::new(InMemoryCheckpointer::<i32>::new());
    let topology = || {
        GraphBuilder::<i32, i32>::overwrite()
            .add_node("a", |s: i32, _c: NodeContext| async move {
                tokio::time::sleep(Duration::from_millis(40)).await;
                Ok(NodeResult::Update(s + 1))
            })
            .add_node("b", |s: i32, _c: NodeContext| async move {
                Ok(NodeResult::Update(s + 1))
            })
            .add_node("c", |s: i32, _c: NodeContext| async move {
                Ok(NodeResult::Update(s + 1))
            })
            .set_entry("a")
            .add_edge("a", "b")
            .add_edge("b", "c")
            .set_finish("c")
            .compile()
            .unwrap()
    };

    // Trips after `a`'s boundary (state=1, next=[b]) but before `b` runs.
    let deadlined = topology()
        .with_checkpointer(cp.clone())
        .with_run_deadline(Duration::from_millis(20));
    let err = deadlined.run_with_thread("t", 0).await.unwrap_err();
    assert!(matches!(err, GraphError::Timeout(_)), "got {err:?}");

    // The boundary checkpoint from the completed super-step survived intact.
    let list = cp.list("t").await.unwrap();
    assert!(
        !list.is_empty(),
        "the pre-deadline boundary checkpoint is intact"
    );

    // Resuming (no deadline) continues from that checkpoint to completion.
    let resumed = topology().with_checkpointer(cp.clone());
    let run = resumed
        .resume("t", Command::resume(json!(null)))
        .await
        .unwrap();
    assert_eq!(run.state, 3, "resume ran the remaining super-steps b and c");
}

// ── Network resilience: node retry + resumable failures ──────────────────────

/// A single-node graph whose handler fails (with a retryable model error) the
/// first `fail_times` invocations, then succeeds with `+1`. The shared counter
/// lets a test observe how many attempts were made.
fn flaky_graph(fail_times: usize, attempts: Arc<AtomicUsize>) -> CompiledGraph<i32, i32> {
    GraphBuilder::<i32, i32>::overwrite()
        .add_node("flaky", move |s, _c: NodeContext| {
            let attempts = attempts.clone();
            async move {
                let n = attempts.fetch_add(1, AtomicOrdering::SeqCst);
                if n < fail_times {
                    Err(GraphError::Graph(format!("transient blip {n}")))
                } else {
                    Ok(NodeResult::Update(s + 1))
                }
            }
        })
        .set_entry("flaky")
        .set_finish("flaky")
        .compile()
        .unwrap()
}

#[tokio::test]
async fn node_failure_without_retry_policy_is_resumable() {
    // No node-retry policy configured: the first failure aborts the run, but a
    // checkpointed thread still leaves a resumable failure boundary (the
    // "resumable abort" default) rather than losing the run.
    let cp = Arc::new(InMemoryCheckpointer::<i32>::new());
    let attempts = Arc::new(AtomicUsize::new(0));
    let graph = flaky_graph(1, attempts.clone()).with_checkpointer(cp.clone());

    let err = graph.run_with_thread("once", 7).await.unwrap_err();
    assert!(matches!(err, GraphError::Graph(_)), "got {err:?}");
    assert_eq!(
        attempts.load(AtomicOrdering::SeqCst),
        1,
        "no retries attempted"
    );

    // Failed status carries the resumable checkpoint id.
    let status = graph.get_state("once", None).await.unwrap().unwrap();
    assert_eq!(status.next_nodes, vec![NodeId::from("flaky")]);

    // The transient condition has cleared; retry completes the run.
    let resumed = graph.retry("once").await.unwrap();
    assert_eq!(resumed.state, 8);
    assert_eq!(resumed.status.status, ExecutionStatus::Completed);
}

#[tokio::test]
async fn edit_state_then_retry_uses_the_edited_state() {
    // User-feedback continuation: after a failure, the operator edits committed
    // state via update_state, then retries; the re-run sees the edited value.
    let cp = Arc::new(InMemoryCheckpointer::<i32>::new());
    let attempts = Arc::new(AtomicUsize::new(0));
    let graph = flaky_graph(1, attempts.clone()).with_checkpointer(cp.clone());

    graph.run_with_thread("feedback", 0).await.unwrap_err();

    // Operator bumps the committed state by +40, inheriting the failure
    // boundary's pending nodes (`flaky`), then retries. The node adds +1 to
    // whatever state it now sees.
    graph.update_state("feedback", 40, None).await.unwrap();
    let resumed = graph.retry("feedback").await.unwrap();
    assert_eq!(
        resumed.state, 41,
        "retry runs against the edited state (40) + 1"
    );
    assert_eq!(resumed.status.status, ExecutionStatus::Completed);
}
