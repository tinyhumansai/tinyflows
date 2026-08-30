use super::super::*;
use super::outcome;

#[derive(Clone)]
pub(super) struct HandlerData {
    pub(super) node: crate::model::Node,
    pub(super) incoming: Vec<(String, String)>,
    pub(super) back_incoming: Vec<(String, String)>,
    pub(super) caps: Capabilities,
    pub(super) agents: Arc<Vec<crate::model::AgentDefinition>>,
    pub(super) observer: Arc<dyn RunObserver>,
    pub(super) steps: Arc<Mutex<Vec<ExecutionStep>>>,
    pub(super) terminal_error: Arc<Mutex<Option<EngineError>>>,
    pub(super) token: CancellationToken,
    pub(super) routing: HandlerRouting,
    pub(super) plain_targets: Vec<String>,
    pub(super) plain_targets_by_port: Vec<(String, Vec<String>)>,
    pub(super) gather_nodes: std::collections::HashSet<String>,
    pub(super) has_error_edge: bool,
    pub(super) is_trigger: bool,
    pub(super) node_timeout: Option<std::time::Duration>,
    /// The step-debugging hook, when one is attached. `None` on every ordinary
    /// run, and the reason interception costs nothing unless asked for: with no
    /// interceptor no [`StepFrame`] is ever built.
    pub(super) interceptor: Option<Arc<dyn StepInterceptor>>,
}

impl HandlerData {
    pub(super) async fn execute(
        self,
        mut state: Value,
        ctx: crate::graph::NodeContext,
    ) -> crate::graph::Result<NodeResult<Value>> {
        let Self {
            node,
            incoming,
            back_incoming,
            caps,
            agents,
            observer,
            steps,
            terminal_error,
            token,
            routing,
            plain_targets,
            plain_targets_by_port,
            gather_nodes,
            has_error_edge,
            is_trigger,
            node_timeout,
            interceptor,
        } = self;

        // The resume value delivered to this node on a checkpointed resume, if
        // any. A bare `true` means "approve the interrupted gate"; a structured
        // `{ "rejected": [<gate id>, …] }` denies the named gate(s).
        let resume_value = ctx.resume.clone();
        // The lane envelope this activation was scheduled with, if it is one
        // of several concurrent activations of this same node. Decoded once
        // here because `ctx` is not moved into the async body below.
        let lane = lane_context(ctx.send_arg.as_ref());
        let lane_send_arg = lane.is_some().then(|| ctx.send_arg.clone()).flatten();
        let activation_step = ctx.step;
        // A checkpointed resume (see `ResumableRun::resume`) delivers a resume
        // value to the interrupted node via `NodeContext::resume`. A resume
        // approves *this* gate only when it is a bare `true` (backward-compat,
        // the single-interrupt case) or when this gate's id is explicitly
        // listed in the structured resume value's `approved` array. A
        // structured resume that names neither this gate in `approved` nor in
        // `rejected` leaves it pending — critical when several parallel gates
        // are interrupted and the host resolves only some of them.
        let approved_by_resume = match ctx.resume.as_ref() {
            Some(Value::Bool(true)) => true,
            Some(v) => v
                .get("approved")
                .and_then(Value::as_array)
                .is_some_and(|approved| {
                    approved
                        .iter()
                        .filter_map(Value::as_str)
                        .any(|id| id == node.id)
                }),
            None => false,
        };
        // Wraps a node's partial update into a routing result. A parallel
        // fan-out drives all of its successors via a `Command::goto`; a
        // port-command node drives only the successors of the port it
        // emitted on (`port`, defaulting to `main`); everything else emits
        // a plain update and follows its static/conditional edge.
        let emit = include!("activation/routing.rs");

        include!("activation/gates.rs");

        // What this activation reads, derived from a given run state.
        //
        // A closure rather than three `let`s because a `Before` interceptor may
        // patch the state, and everything downstream — the input items, the
        // expression scope, the resolved config — has to be re-derived from the
        // patched state or the node would run against one state while the
        // debugger showed another.
        let derive = include!("activation/input.rs");
        let (mut input, mut run_meta, mut nodes_state) = derive(&state);

        // Per-node error policy, read from free-form `node.config` (no model
        // struct change). `on_error` selects what happens once retries are
        // exhausted; `retry.max_attempts` bounds the attempts.
        let on_error = node
            .config
            .get("on_error")
            .and_then(Value::as_str)
            .unwrap_or("stop");
        let max_attempts = node
            .config
            .get("retry")
            .and_then(|retry| retry.get("max_attempts"))
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .max(1);
        // Backoff between attempts. `backoff_ms` is the base delay (default
        // 0 = no wait); `backoff` selects `"fixed"` (default, constant delay)
        // or `"exponential"` (`backoff_ms * 2^attempt_index`). We use a
        // runtime-agnostic timer (`futures_timer::Delay`) so the crate stays
        // host-agnostic and pulls in no specific async runtime.
        let backoff_ms = node
            .config
            .get("retry")
            .and_then(|retry| retry.get("backoff_ms"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let exponential = node
            .config
            .get("retry")
            .and_then(|retry| retry.get("backoff"))
            .and_then(Value::as_str)
            == Some("exponential");

        // Attempt the executor up to `max_attempts` times: use the first
        // `Ok`, otherwise keep the last `Err`. Between failed attempts (never
        // after the final one), wait for the configured backoff delay.
        // How finely the backoff sleep is chopped so a cancel mid-backoff
        // is observed promptly instead of after the whole delay elapses.
        const BACKOFF_POLL_MS: u64 = 25;

        // The pre-execution interception point.
        //
        // Deliberately here and nowhere else:
        //  - *after* the cancellation check and the approval gate, so an
        //    interceptor is never shown an activation the engine had already
        //    decided to skip or pause;
        //  - *after* input resolution, so the frame carries the node's real
        //    items and its real bindings rather than a promise of them;
        //  - *outside* the retry loop, so it fires once per activation instead
        //    of once per attempt — a breakpoint that fired three times because
        //    the node was configured with `retry.max_attempts: 3` would be
        //    reporting an implementation detail as a decision;
        //  - *outside* the per-attempt timeout race below, so time spent parked
        //    at a breakpoint is never charged against `node_timeout_secs`.
        //
        // A substituted output skips the executor entirely and then rejoins the
        // ordinary path, so it is recorded as a step, reported to the observer,
        // and routed through the same port and lane logic as real output.
        let mut state_patch: Option<Value> = None;
        let mut substituted: Option<crate::nodes::NodeOutput> = None;
        let mut injected_err: Option<EngineError> = None;
        if let Some(hook) = interceptor.as_ref() {
            let action = hook
                .intercept(StepFrame {
                    phase: StepPhase::Before,
                    node: &node,
                    step: activation_step,
                    attempts: 0,
                    input: &input,
                    run: &run_meta,
                    nodes: &nodes_state,
                    state: &state,
                    lane: lane.as_ref(),
                    resume: resume_value.as_ref(),
                    output: None,
                    error: None,
                })
                .await;
            match action {
                StepAction::Continue { state_patch: None } => {}
                StepAction::Continue {
                    state_patch: Some(patch),
                } => {
                    merge(&mut state, patch.clone());
                    let (i, r, n) = derive(&state);
                    input = i;
                    run_meta = r;
                    nodes_state = n;
                    state_patch = Some(patch);
                }
                StepAction::Replace { items, port } => {
                    substituted = Some(crate::nodes::NodeOutput {
                        port,
                        ..crate::nodes::NodeOutput::main(items)
                    });
                }
                StepAction::Skip => {
                    substituted = Some(crate::nodes::NodeOutput::empty());
                }
                StepAction::Fail { message } => {
                    injected_err = Some(EngineError::Capability(message));
                }
                StepAction::Interrupt { id, payload } => {
                    tracing::info!(node = %node.id, gate = %id, "interceptor paused the run");
                    return Ok(NodeResult::Interrupt(Interrupt {
                        id,
                        node: node.id.clone().into(),
                        payload,
                    }));
                }
            }
        }

        observer.on_step_start(&node.id);
        let mut output = substituted;
        let mut last_err: Option<EngineError> = injected_err;
        let mut attempts_used: u32 = 0;
        let started = Instant::now();
        // An interceptor that substituted an output or injected a failure has
        // already decided this activation's outcome, so the executor does not
        // run — and an injected failure does not consume retry attempts, which
        // is what keeps a debugger from sitting through synthetic ones.
        let run_executor = output.is_none() && last_err.is_none();
        // An interceptor may give this activation its own capability bundle, so
        // that every outside call it makes can be attributed to *this* node —
        // which nothing else can recover once several nodes are calling at once.
        // Resolved once per activation rather than per attempt: a retry is the
        // same node calling again, not a different caller.
        let caps = match interceptor
            .as_ref()
            .and_then(|hook| hook.capabilities_for(&node, &caps))
        {
            Some(scoped) => scoped,
            None => caps,
        };
        for attempt in 0..(if run_executor { max_attempts } else { 0 }) {
            attempts_used = u32::try_from(attempt + 1).unwrap_or(u32::MAX);
            // Cooperative cancellation inside the retry loop. Without this a
            // node with a large `max_attempts`/`backoff_ms` keeps retrying
            // and sleeping through its whole budget after `cancel()`; check
            // before starting each attempt and bail the same way the
            // node-boundary check does — an empty no-op update, so the run
            // winds down fast and reports cancelled.
            if token.is_cancelled() {
                tracing::info!(node = %node.id, "run cancelled during retry; skipping remaining attempts");
                return Ok(NodeResult::Update(items_update(&node.id, &[], None)?));
            }
            let ctx = NodeContext {
                node: &node,
                input: &input,
                run: &run_meta,
                nodes: &nodes_state,
                caps: &caps,
                agents: &agents,
                observer: observer.as_ref(),
                // Handed to the executor so a nested engine call (today the
                // `sub_workflow` node) can thread this run's cancellation
                // into its child; a plain executor never reads it.
                token: token.clone(),
                lane: lane.clone(),
                resume: resume_value.clone(),
                step: activation_step,
            };
            // BUG-8: bound THIS attempt (not the whole retry loop) to
            // `node_timeout`. Race the attempt future against a
            // runtime-agnostic `futures_timer::Delay`; whichever resolves
            // first wins (`None` = the timer won, i.e. the attempt timed
            // out). When `node_timeout` is unset the attempt is awaited
            // directly, identical to before this fix.
            let attempt_result = match node_timeout {
                Some(timeout) => {
                    let executor = executor_for(&node.kind);
                    let attempt_fut = executor.execute(ctx);
                    let mut attempt_fut = std::pin::pin!(attempt_fut);
                    let mut timer = futures_timer::Delay::new(timeout);
                    std::future::poll_fn(|cx| {
                        use std::future::Future as _;
                        if let std::task::Poll::Ready(out) = attempt_fut.as_mut().poll(cx) {
                            return std::task::Poll::Ready(Some(out));
                        }
                        if std::pin::Pin::new(&mut timer).poll(cx).is_ready() {
                            return std::task::Poll::Ready(None);
                        }
                        std::task::Poll::Pending
                    })
                    .await
                }
                None => Some(executor_for(&node.kind).execute(ctx).await),
            };
            match attempt_result {
                Some(Ok(ok)) => {
                    output = Some(ok);
                    break;
                }
                Some(Err(err)) => last_err = Some(err),
                // A timed-out attempt counts as a failed attempt: record
                // it as an error so the retry/`on_error` logic treats it
                // like any other failure (retry if attempts remain, else
                // apply `on_error`).
                None => {
                    let secs = node_timeout.map(|d| d.as_secs()).unwrap_or_default();
                    tracing::warn!(node = %node.id, attempt = attempt + 1, timeout_secs = secs, "node attempt timed out");
                    last_err = Some(EngineError::Capability(format!(
                        "node '{}' attempt {} timed out after {}s",
                        node.id,
                        attempt + 1,
                        secs
                    )));
                }
            }
            // Sleep before the next attempt, but not after the last one.
            if backoff_ms > 0 && attempt + 1 < max_attempts {
                // `attempt` is the 0-based index of the attempt that just
                // failed; exponential grows the base by `2^attempt`. All
                // math saturates and the delay is capped at 60s so a large
                // `backoff_ms`/attempt count can never overflow or hang.
                let delay = if exponential {
                    2u64.checked_pow(attempt as u32)
                        .and_then(|factor| backoff_ms.checked_mul(factor))
                        .unwrap_or(u64::MAX)
                } else {
                    backoff_ms
                }
                .min(60_000);
                // Cancellable backoff: sleep in small increments, checking
                // the cancellation token between them, so a cancel during
                // the wait stops the run promptly. The total delay for a
                // non-cancelled run is unchanged.
                let mut remaining = delay;
                while remaining > 0 {
                    if token.is_cancelled() {
                        tracing::info!(node = %node.id, "run cancelled during backoff; skipping remaining attempts");
                        return Ok(NodeResult::Update(items_update(&node.id, &[], None)?));
                    }
                    let step = remaining.min(BACKOFF_POLL_MS);
                    // Yields even when the timer has already fired, so a retry
                    // backoff never starves the executor it shares — see
                    // `super::backoff`.
                    super::backoff::wait_slice(step).await;
                    remaining -= step;
                }
            }
        }
        let duration_ms = started.elapsed().as_millis();

        // The post-execution interception point.
        //
        // Placed here — after the retry loop has settled, before
        // `finish_execution` builds the `ExecutionStep` — so a rewritten
        // outcome is what the run *records* and what `on_step_finish` reports.
        // Intercepting any later would leave the observer and the committed
        // state disagreeing about what the node produced.
        //
        // This is the only phase that can see a failure, and the only one that
        // can rescue one: replacing a failed activation's output turns it into
        // a success and bypasses `on_error` entirely, which is the point of
        // "pretend that flaky call returned this".
        if let Some(hook) = interceptor.as_ref() {
            let action = hook
                .intercept(StepFrame {
                    phase: StepPhase::After,
                    node: &node,
                    step: activation_step,
                    attempts: attempts_used,
                    input: &input,
                    run: &run_meta,
                    nodes: &nodes_state,
                    state: &state,
                    lane: lane.as_ref(),
                    resume: resume_value.as_ref(),
                    output: output.as_ref(),
                    // Only when the activation actually failed. The retry loop
                    // keeps the last failed attempt's error even after a later
                    // attempt succeeds, so reporting `last_err` unconditionally
                    // would show a node that recovered as one that failed — and
                    // fire every on-error breakpoint on it.
                    error: output.is_none().then_some(last_err.as_ref()).flatten(),
                })
                .await;
            match action {
                // A patch at `After` is ignored: the state this activation will
                // write is already decided, so honouring one would misreport
                // what the node actually saw.
                StepAction::Continue { .. } => {}
                StepAction::Replace { items, port } => {
                    let mut replaced = output
                        .take()
                        .unwrap_or_else(crate::nodes::NodeOutput::empty);
                    replaced.items = items;
                    replaced.port = port;
                    // A control request belonged to the output the node really
                    // produced. An overridden output is plain data, so it must
                    // not still scatter, re-enter, or interrupt on its behalf.
                    replaced.control = None;
                    output = Some(replaced);
                    last_err = None;
                }
                StepAction::Skip => {
                    let mut skipped = output
                        .take()
                        .unwrap_or_else(crate::nodes::NodeOutput::empty);
                    skipped.items.clear();
                    skipped.control = None;
                    output = Some(skipped);
                    last_err = None;
                }
                StepAction::Fail { message } => {
                    output = None;
                    last_err = Some(EngineError::Capability(message));
                }
                StepAction::Interrupt { id, payload } => {
                    tracing::info!(node = %node.id, gate = %id, "interceptor paused the run");
                    return Ok(NodeResult::Interrupt(Interrupt {
                        id,
                        node: node.id.clone().into(),
                        payload,
                    }));
                }
            }
        }

        let result = outcome::finish_execution(
            output,
            last_err,
            attempts_used,
            duration_ms,
            on_error,
            &node,
            &input,
            &run_meta,
            &nodes_state,
            &caps,
            &agents,
            &observer,
            &token,
            &lane,
            &resume_value,
            activation_step,
            &steps,
            &terminal_error,
            &plain_targets,
            emit,
        )
        .await?;

        // A `Before` state patch is merged into whatever this activation
        // commits, so an edit made at a breakpoint survives the node writing
        // its own slot rather than being silently overwritten by it.
        Ok(match state_patch {
            Some(patch) => apply_state_patch(result, patch),
            None => result,
        })
    }
}

/// Merge a `Before` interceptor's state patch into an activation's committed
/// update.
///
/// An interrupt is left alone: the runtime discards an interrupting
/// activation's update by design, so there is nothing here to merge into and
/// pretending otherwise would claim a write that never lands.
fn apply_state_patch(result: NodeResult<Value>, patch: Value) -> NodeResult<Value> {
    match result {
        NodeResult::Update(mut update) => {
            merge(&mut update, patch);
            NodeResult::Update(update)
        }
        NodeResult::Command(mut command) => {
            match command.update.take() {
                Some(mut update) => {
                    merge(&mut update, patch);
                    command.update = Some(update);
                }
                None => command.update = Some(patch),
            }
            NodeResult::Command(command)
        }
        interrupt => interrupt,
    }
}
