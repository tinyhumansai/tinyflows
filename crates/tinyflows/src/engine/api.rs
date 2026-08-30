use super::*;

/// Executes a compiled workflow with the given trigger `input` and host
/// `capabilities`, driving it to completion.
///
/// This installs a no-op [`RunObserver`]; use [`run_with_observer`] to receive
/// run/step observability records as the run executes.
///
/// # Errors
/// Returns an [`EngineError`] if lowering, compilation, or execution fails —
/// including any error a node's executor produces. A node kind whose executor is
/// not yet implemented surfaces its `Unimplemented` error here.
pub async fn run(
    workflow: &CompiledWorkflow,
    input: impl Into<RunInput>,
    capabilities: &Capabilities,
) -> Result<RunOutcome> {
    run_with_observer(
        workflow,
        input,
        capabilities,
        &(Arc::new(crate::observability::NoopObserver) as Arc<dyn RunObserver>),
    )
    .await
}

/// Like [`run`], but reports run/step records to `observer` as the run executes:
/// [`RunObserver::on_run_start`] fires once before any node runs,
/// [`RunObserver::on_step_finish`] once per non-trigger node as it finishes, and
/// [`RunObserver::on_run_finish`] once with the assembled [`Run`]. All execution
/// behavior (retry, `on_error`, HITL interrupts, conditional routing, tracing) is
/// identical to [`run`].
///
/// # Errors
/// Same as [`run`].
pub async fn run_with_observer(
    workflow: &CompiledWorkflow,
    input: impl Into<RunInput>,
    capabilities: &Capabilities,
    observer: &Arc<dyn RunObserver>,
) -> Result<RunOutcome> {
    // Default (non-injectable) path: a process-local in-memory checkpointer,
    // keyed by the trigger id — identical behavior to before checkpointer
    // injection existed.
    let (_graph, _thread_id, outcome, _run_ids) = build_and_run(
        workflow,
        input,
        capabilities,
        observer,
        RunConfig::new(workflow)?,
    )
    .await?;
    Ok(outcome)
}

/// Like [`run`], but observes `token`: cancelling it stops the run from
/// scheduling further node work at the next node boundary, and the returned
/// [`RunOutcome`] has [`cancelled`](RunOutcome::cancelled) set. A node already
/// executing when the token flips finishes; no *new* node work starts after
/// cancellation. All other behavior is identical to [`run`].
///
/// This is the clean, engine-level cooperative-cancellation path, complementing a
/// host's hard task-abort: the run winds down and returns a partial outcome rather
/// than being dropped mid-await.
///
/// # Errors
/// Same as [`run`].
pub async fn run_cancellable(
    workflow: &CompiledWorkflow,
    input: impl Into<RunInput>,
    capabilities: &Capabilities,
    token: CancellationToken,
) -> Result<RunOutcome> {
    let observer = Arc::new(crate::observability::NoopObserver) as Arc<dyn RunObserver>;
    run_cancellable_with_observer(workflow, input, capabilities, token, &observer).await
}

/// Like [`run_cancellable`], while also reporting lifecycle and step records to
/// `observer` as they settle.
///
/// # Errors
/// Same as [`run_cancellable`].
pub async fn run_cancellable_with_observer(
    workflow: &CompiledWorkflow,
    input: impl Into<RunInput>,
    capabilities: &Capabilities,
    token: CancellationToken,
    observer: &Arc<dyn RunObserver>,
) -> Result<RunOutcome> {
    let (_graph, _thread_id, outcome, _run_ids) = build_and_run(
        workflow,
        input,
        capabilities,
        observer,
        RunConfig::new(workflow)?.with_token(token),
    )
    .await?;
    Ok(outcome)
}

/// The maximum nesting depth for `sub_workflow` runs.
///
/// Each nested `sub_workflow` run (inline **or** by `workflow_id`) increments a
/// `run.sub_workflow_depth` counter; once a child would exceed this bound the
/// `sub_workflow` node refuses to run it. This is the engine's backstop against
/// runaway or cyclic references (e.g. flow A → flow B → flow A by id): the chain
/// is cut after at most this many levels regardless of how the cycle is formed.
/// A direct self-reference is additionally caught statically by the node before
/// any run starts (see [`crate::nodes::integration::SubWorkflowNode`]).
///
/// This is the **default**, not a hard ceiling: a graph that legitimately nests
/// deeper sets `max_sub_workflow_depth` on its trigger config, which is seeded
/// into the run state and forwarded to every child run so the whole chain
/// agrees on one bound.
pub const MAX_SUB_WORKFLOW_DEPTH: u64 = 8;

/// The ceiling on `trigger.config.max_concurrency`: how many branches of one
/// super-step the engine will ever run at once.
///
/// A cap rather than an error, mirroring how a node's own `concurrency` is
/// clamped: a graph asking for 100_000 concurrent branches has a mistake in it,
/// and refusing the run outright would be a worse answer than running it
/// sensibly and saying so in a warning.
pub const MAX_GRAPH_CONCURRENCY: usize = 256;

/// Runs a nested child workflow for a `sub_workflow` node, threading the current
/// nesting `depth` into the child run's `run.sub_workflow_depth`.
///
/// Behaves like [`run`] (no-op observer, process-local in-memory checkpointer)
/// but seeds the depth counter so a further nested `sub_workflow` inside the
/// child can read it back from `ctx.run` and enforce [`MAX_SUB_WORKFLOW_DEPTH`].
/// Used only by the `sub_workflow` node's recursive execution.
///
/// `token` is the **parent run's** cancellation token, forwarded so cancelling
/// the parent winds the whole subtree down: the child observes the same flipped
/// flag at its next node boundary and returns a cancelled [`RunOutcome`] instead
/// of running to completion orphaned from the parent. The child in turn hands
/// this token to its own node contexts, so a deeper `sub_workflow` propagates it
/// on — the whole nesting chain shares one signal. Historically this seeded a
/// fresh [`CancellationToken`], which severed cancellation at every sub-workflow
/// boundary.
///
/// # Errors
/// Same as [`run`].
pub(crate) async fn run_sub_workflow(
    workflow: &CompiledWorkflow,
    input: impl Into<RunInput>,
    capabilities: &Capabilities,
    depth: u64,
    max_depth: u64,
    token: CancellationToken,
    workspace: Option<String>,
) -> Result<RunOutcome> {
    let observer: Arc<dyn RunObserver> = Arc::new(crate::observability::NoopObserver);
    let mut overlay = json!({
        "sub_workflow_depth": depth,
        "max_sub_workflow_depth": max_depth,
    });
    // The child's workspace: the parent's, unless the `sub_workflow` node named
    // another (already resolved and contained by the node). Inherited rather
    // than dropped, because a child whose agents suddenly resolved their `cwd`
    // against nothing would run them in whatever directory the harness defaults
    // to — the silent relocation this whole seam exists to prevent.
    if let Some(workspace) = workspace {
        overlay["workspace"] = Value::from(workspace);
    }
    let (_graph, _thread_id, outcome, _run_ids) = build_and_run(
        workflow,
        input,
        capabilities,
        &observer,
        RunConfig::new(workflow)?
            .with_token(token)
            .with_overlay(overlay),
    )
    .await?;
    Ok(outcome)
}
