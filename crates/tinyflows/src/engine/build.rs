use super::*;

mod activation;
mod backoff;
mod handlers;
mod outcome;
mod wiring;

/// Builds and compiles the graph-runtime graph for `workflow`, attaching the
/// host-supplied `checkpointer`, and returns the compiled graph together with
/// the graph's entry (trigger) node id.
///
/// Node handlers capture `observer` (to fire `on_step_finish`) and the shared
/// `steps` sink. This does **not** run the graph — callers either drive it (via
/// `run_with_thread`, see [`build_and_run`]) or resume it from a persisted
/// checkpoint (via `resume`, see [`resume_with_checkpointer`]). Keeping graph
/// construction separate is what lets a host rebuild the identical graph in a
/// fresh process, re-attach the same durable `checkpointer`, and resume.
///
/// When `journal` is supplied the compiled graph is additionally wired with
/// the runtime's durable event journal (see
/// [`CompiledGraph::with_event_journal`]), so every emitted graph event is
/// recorded as a [`GraphObservation`] keyed by the run's graph run id.
///
/// # Errors
/// Returns an [`EngineError`] if the workflow has no trigger or if compilation
/// fails.
pub(super) fn build_graph(
    workflow: &CompiledWorkflow,
    capabilities: &Capabilities,
    observer: &Arc<dyn RunObserver>,
    steps: &Arc<Mutex<Vec<ExecutionStep>>>,
    terminal_error: &Arc<Mutex<Option<EngineError>>>,
    config: &RunConfig,
) -> Result<(CompiledGraph<Value, Value>, String)> {
    let checkpointer = config.checkpointer.clone();
    let journal = config.journal.clone();
    let token = config.token.clone();
    let graph = &workflow.graph;

    let trigger = graph
        .trigger()
        .ok_or(EngineError::Validation(ValidationError::MissingTrigger))?;
    let trigger_id = trigger.id.clone();

    // Run-level knobs are read from the trigger node's config — the natural
    // run-level config holder, since `WorkflowGraph` has no top-level config.
    // `recursion_limit` bounds loops (the runtime's default is 50) and is applied
    // to the builder below; `node_timeout_secs` bounds each individual node
    // *attempt* and is applied per attempt inside the handler's retry loop.
    let recursion_limit = trigger
        .config
        .get("recursion_limit")
        .and_then(Value::as_u64)
        .filter(|n| *n > 0);
    // BUG-8: `node_timeout_secs` bounds EACH executor attempt, not the whole
    // retry loop. It is deliberately NOT lowered to a graph-level
    // `with_node_timeout` (which wraps a node handler's *entire* execution — all
    // attempts plus their backoff sleeps — so a 1s timeout on a node with 2
    // retries would kill it at 1s instead of giving each attempt its own 1s).
    // Instead the value is captured into every node handler and raced against
    // each attempt inside the retry loop below. `None` (unset) => no per-attempt
    // timeout, behavior identical to before this fix.
    let node_timeout = trigger
        .config
        .get("node_timeout_secs")
        .and_then(Value::as_u64)
        .filter(|n| *n > 0)
        .map(std::time::Duration::from_secs);
    // How many branches of one super-step may be in flight at once.
    //
    // This is *admission control*, not backpressure: a super-step engine cannot
    // block a producer mid-step, so the only lever is how many activations are
    // allowed to start. Unset means unbounded, which is the historical behavior.
    let max_concurrency = trigger
        .config
        .get("max_concurrency")
        .and_then(Value::as_u64)
        .filter(|n| *n > 0)
        .map(|requested| {
            let requested = usize::try_from(requested).unwrap_or(MAX_GRAPH_CONCURRENCY);
            if requested > MAX_GRAPH_CONCURRENCY {
                tracing::warn!(
                    requested,
                    max = MAX_GRAPH_CONCURRENCY,
                    "max_concurrency above the engine ceiling; clamping"
                );
                MAX_GRAPH_CONCURRENCY
            } else {
                requested
            }
        });

    tracing::info!(node_count = graph.nodes.len(), trigger = %trigger_id, "workflow run starting");

    // The edges that close a cycle. Everything below that reasons about fan-in
    // must ignore these: a back-edge is not a predecessor the node waits for,
    // it is the node's own re-entry. See [`back_edges`].
    let loop_edges = back_edges(graph);

    // How each node drives its successors (parallel fan-out, port-selective
    // command routing, or plain edge following) is derived from its outgoing-edge
    // shape by [`handler_routing`]; the handler and the lowering below both key
    // off it so they stay in agreement.

    // Concurrency is required so a fan-out's successors execute in the same
    // superstep; the reducer folds their independent, per-id updates
    // deterministically, so enabling it never changes a linear run's result.
    let mut builder = GraphBuilder::<Value, Value>::new()
        .with_parallel(true)
        .set_reducer(MergeReducer);
    if let Some(limit) = max_concurrency {
        builder = builder.with_max_concurrency(limit);
    }
    if let Some(limit) = recursion_limit {
        builder = builder.with_recursion_limit(limit as usize);
    } else {
        // The graph runtime defaults to 50 super-steps, which is too small for the
        // advertised default loop cap once the head and body activations are
        // counted. A declared loop already has its own finite cap, so derive a
        // conservative graph budget that lets that structured limit win.
        let loop_budget: u64 = graph
            .nodes
            .iter()
            .filter(|node| node.kind == NodeKind::Loop)
            .map(|node| {
                node.config
                    .get("max_iterations")
                    .and_then(Value::as_u64)
                    .unwrap_or(crate::nodes::control_flow::DEFAULT_MAX_ITERATIONS)
                    .saturating_add(1)
            })
            .sum();
        if loop_budget > 0 {
            let derived = loop_budget
                .saturating_mul(graph.nodes.len() as u64)
                .saturating_add(graph.nodes.len() as u64);
            builder = builder.with_recursion_limit(derived.min(usize::MAX as u64) as usize);
        }
    }
    // NOTE: `node_timeout` is intentionally NOT wired via
    // `builder.with_node_timeout(..)`. That runtime knob wraps a node
    // handler's *entire* execution — the whole retry loop, including backoff
    // sleeps — which is exactly the BUG-8 conflation of the timeout and retry
    // budgets. The per-attempt bound is applied inside the handler's retry loop
    // instead (raced against each attempt), so it is the sole timeout in effect.

    builder = handlers::register_handlers(
        builder,
        graph,
        capabilities,
        observer,
        steps,
        terminal_error,
        &token,
        node_timeout,
        &loop_edges,
        config.interceptor.as_ref(),
    );

    builder = wiring::wire_graph(builder, graph, &trigger_id, &loop_edges);

    // A checkpointer (plus a thread id on the run) is required for the graph runtime to
    // persist the interrupt boundary and hand pending approvals back to us. The
    // checkpointer is host-injected: the default entry points supply an
    // in-memory one to keep the crate host-agnostic and dep-free, while a host
    // can inject a durable store for cross-process resume.
    let mut compiled = builder
        .compile()
        .map_err(|e| EngineError::Capability(e.to_string()))?
        .with_checkpointer(checkpointer);

    // Opt-in durable observability: with a journal attached, the runtime wraps
    // every emitted graph event into a `GraphObservation` (stamped with run
    // lineage + step) and appends it under the run id.
    if let Some(journal) = journal {
        tracing::debug!("attaching graph event journal to compiled workflow graph");
        compiled = compiled.with_event_journal(journal);
    }

    // `max_node_visits` caps how many times any single node may be activated
    // within one run — the last-resort backstop under a `loop` node's own
    // `max_iterations`. It exists because the two limits fail differently:
    // `max_iterations` is per-loop and names the loop that ran away, whereas
    // this bounds a cycle nobody declared a loop node for (and, unlike
    // `recursion_limit`, it names the offending node rather than just reporting
    // that the run took too many super-steps).
    //
    // Only set when asked. `RecursionPolicy` also carries `max_total_steps`,
    // and the runtime takes `min(recursion_limit, max_total_steps)` — so
    // installing a default policy here would silently tighten the step budget
    // of every existing graph.
    if let Some(max_visits) = trigger
        .config
        .get("max_node_visits")
        .and_then(Value::as_u64)
        .filter(|n| *n > 0)
    {
        let defaults = crate::graph::RecursionPolicy::default();
        compiled = compiled.with_recursion_policy(crate::graph::RecursionPolicy {
            max_visits_per_node: Some(max_visits as usize),
            // Keep whatever step budget `recursion_limit` asked for; the
            // policy's own default (1000) must not quietly become a second,
            // lower ceiling.
            max_total_steps: recursion_limit.map_or(defaults.max_total_steps, |limit| {
                defaults.max_total_steps.max(limit as usize)
            }),
            ..defaults
        });
    }

    Ok((compiled, trigger_id))
}
