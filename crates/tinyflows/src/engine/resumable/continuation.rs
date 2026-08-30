impl Continuation {
    /// The command that carries this continuation into the runtime.
    fn command(self) -> Command<Value> {
        match self {
            Self::Approvals { approved, rejected } => {
                // Approvals recorded for downstream visibility. On resume the
                // interrupted gate is approved because the resume value reaches
                // it via `NodeContext::resume`; the `with_update` mirrors
                // `ResumableRun::resume` (the runtime ignores it on resume, so
                // the resume value is the real approval channel).
                let update = json!({
                    "run": { "trigger": { "approvals": approved.clone() } }
                });
                if !rejected.is_empty() {
                    tracing::info!(?rejected, "resuming with denied approval gate(s)");
                }
                // Always a structured resume value carrying the explicit
                // `approved` and `rejected` gate id lists. Each interrupted gate
                // decides for itself: gates in `approved` proceed, gates in
                // `rejected` route to their `error` port (or fail), and gates in
                // neither stay pending. This is essential when several parallel
                // gates are interrupted and the host resolves only some of them
                // — a bare `true` would blanket-approve every interrupt
                // regardless of the host's decision.
                let value = json!({ "approved": approved, "rejected": rejected });
                Command::resume(value).with_update(update)
            }
            // Deliberately empty. A failed node is re-entered from its start
            // with the state the boundary committed; a resume *value* would be
            // delivered to `NodeContext::resume` and read as an approval
            // decision by any gate that happened to be in the pending set.
            Self::Retry => Command::new(),
        }
    }
}

/// Shared implementation of the checkpointed continue path: rebuilds the graph
/// (optionally journaled), re-attaches the same `checkpointer`, and resumes
/// `thread_id`. Returns the outcome plus the resumed execution's
/// runtime-minted run ids.
async fn resume_with_checkpointer_inner(
    workflow: &CompiledWorkflow,
    capabilities: &Capabilities,
    checkpointer: Arc<dyn Checkpointer<Value>>,
    thread_id: &str,
    continuation: Continuation,
    journal: Option<Arc<dyn GraphEventJournal>>,
    observer: &Arc<dyn RunObserver>,
) -> Result<(RunOutcome, GraphRunIds)> {
    let steps: Arc<Mutex<Vec<ExecutionStep>>> = Arc::new(Mutex::new(Vec::new()));

    // Rebuild the identical graph and re-attach the SAME checkpointer, so
    // `resume` loads the state persisted under `thread_id`. Node handlers fire
    // `observer.on_step_finish` for every node that runs after the interrupt
    // boundary, so a host observer sees the resumed steps live.
    let terminal_error: Arc<Mutex<Option<EngineError>>> = Arc::new(Mutex::new(None));
    let mut config = RunConfig::new(workflow)?.with_checkpointer(checkpointer, thread_id);
    if let Some(journal) = journal {
        config = config.with_journal(journal);
    }
    let (compiled, _trigger_id) = build_graph(
        workflow,
        capabilities,
        observer,
        &steps,
        &terminal_error,
        &config,
    )?;

    let execution = compiled.resume(thread_id, continuation.command()).await;
    let execution = match execution {
        Ok(execution) => execution,
        Err(error) => {
            let structured = terminal_error
                .lock()
                .expect("terminal error mutex poisoned")
                .take();
            return Err(structured.unwrap_or_else(|| EngineError::Capability(error.to_string())));
        }
    };

    let pending_approvals: Vec<String> = execution
        .interrupts
        .iter()
        .map(|interrupt| interrupt.id.clone())
        .collect();

    let graph_run_ids = GraphRunIds {
        run_id: execution.run_id.as_str().to_string(),
        root_run_id: execution.root_run_id.as_str().to_string(),
    };

    Ok((
        RunOutcome {
            output: execution.state,
            pending_approvals,
            // Checkpointed resume does not (yet) thread a caller token; a
            // cancellable resume goes through `resume_cancellable`.
            cancelled: false,
        },
        graph_run_ids,
    ))
}

/// What a failed run left behind, and what it would take to continue it.
///
/// A run that fails does not necessarily lose its work. On a checkpointed
/// thread the runtime folds the branches that already completed into committed
/// state and writes a **failure boundary** — a checkpoint whose pending nodes
/// are the node that failed and the not-yet-run tail of its step. Everything
/// before it is durable and does not have to happen twice.
///
/// The engine reports the failure as an `Err`, which is the right shape for a
/// caller that just wants to know the run did not finish. This is the question
/// that error cannot answer: *is there something to continue, and where did it
/// stop?* Read it after a failed run to decide between fixing and retrying,
/// and re-running from the trigger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureBoundary {
    /// The node whose handler failed.
    pub failed_node: String,
    /// The error as the runtime rendered it, for diagnosis.
    pub error: String,
    /// The checkpoint holding the committed prefix — the id
    /// [`ResumeTarget::Checkpoint`](crate::graph::ResumeTarget) addresses.
    pub checkpoint_id: String,
    /// Which superstep the run reached.
    pub step: usize,
    /// The nodes a continue would run: the failed one, and whatever else in
    /// its step had not run when it aborted.
    pub pending: Vec<String>,
}

/// Read the failure boundary a thread's latest checkpoint records, if it is one.
///
/// `Ok(None)` for a thread that has no checkpoint, or whose latest is an
/// ordinary boundary — a completed run, or one paused at an approval gate.
/// Those are not failures and have nothing to continue *from a failure*.
///
/// Deliberately a separate read rather than a field on the error. A failed run
/// already returns [`EngineError`], every caller handles that, and widening it
/// would make every one of them carry a concept most do not use. Asking
/// afterwards also reads the way the decision is actually made: the run
/// failed — is it worth continuing?
///
/// # Errors
/// When the checkpointer cannot be read.
pub async fn failure_boundary(
    checkpointer: &Arc<dyn Checkpointer<Value>>,
    thread_id: &str,
) -> Result<Option<FailureBoundary>> {
    let checkpoint = checkpointer
        .get(thread_id, None)
        .await
        .map_err(|error| EngineError::Capability(error.to_string()))?;
    let Some(checkpoint) = checkpoint else {
        return Ok(None);
    };
    // `failed_node` is what makes a boundary a *failure* boundary — an
    // interrupt boundary and a terminal one both lack it. Reading the key
    // rather than a status field keeps this to one checkpoint load.
    let Some(failed_node) = checkpoint
        .metadata
        .get("failed_node")
        .and_then(Value::as_str)
    else {
        return Ok(None);
    };
    Ok(Some(FailureBoundary {
        failed_node: failed_node.to_string(),
        error: checkpoint
            .metadata
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        checkpoint_id: checkpoint.checkpoint_id.clone(),
        step: checkpoint
            .metadata
            .get("step")
            .and_then(Value::as_u64)
            .and_then(|step| usize::try_from(step).ok())
            .unwrap_or(0),
        pending: checkpoint
            .next_nodes
            .iter()
            .map(ToString::to_string)
            .collect(),
    }))
}

/// Continue a **failed** run from where it stopped: re-run the node that
/// failed and the not-yet-run tail of its step, on the state the failure
/// boundary committed.
///
/// The counterpart of [`resume_with_checkpointer`] for the failure path. That
/// one answers a pause with a decision; this one answers a break with another
/// go, and carries no resume value — there is nothing to decide, only work to
/// redo.
///
/// Two reasons to reach for this over re-running the workflow:
///
/// * **Side effects.** A prefix that posted a comment, opened a pull request
///   or charged something does not do it twice. Re-running from the trigger is
///   not a neutral choice for a graph with effects in it; it is a second set
///   of them.
/// * **Cost.** A prefix step can be a whole coding session. Paying for it
///   again to reach the same failed node buys nothing.
///
/// **The graph must be the one that failed.** Node handlers are rebuilt from
/// `workflow`, and the committed state is keyed by node id, so a `workflow`
/// whose prefix differs from the one that ran will re-enter the tail on state
/// it would never have produced — a run that goes green and is quietly wrong.
/// Editing a *later* node is the supported case and the useful one: fix the
/// step that failed, continue, keep the prefix. A caller that changed anything
/// at or upstream of `failed_node` must re-run from the trigger instead, and
/// [`failure_boundary`] names that node so the check is possible.
///
/// # Errors
/// [`EngineError::Capability`] when the thread has no checkpoint, or the
/// checkpoint schedules nothing to run — a completed run has no tail, and
/// asking it to continue is a caller mistake worth naming rather than a
/// silently empty outcome. Otherwise as [`run`].
pub async fn retry_with_checkpointer(
    workflow: &CompiledWorkflow,
    capabilities: &Capabilities,
    checkpointer: Arc<dyn Checkpointer<Value>>,
    thread_id: &str,
) -> Result<RunOutcome> {
    let observer = Arc::new(crate::observability::NoopObserver) as Arc<dyn RunObserver>;
    let (outcome, _run_ids) = resume_with_checkpointer_inner(
        workflow,
        capabilities,
        checkpointer,
        thread_id,
        Continuation::Retry,
        None,
        &observer,
    )
    .await?;
    Ok(outcome)
}

/// Like [`retry_with_checkpointer`], but journaled and observed — the shape a
/// host that records runs actually needs.
///
/// The journaled counterpart of
/// [`resume_with_checkpointer_journaled_observed`], and for the same reason: a
/// host whose run records are built from observed steps must see the continued
/// leg the same way it saw the first one, or the record it writes claims the
/// tail never ran.
///
/// # Errors
/// Same as [`retry_with_checkpointer`].
pub async fn retry_with_checkpointer_journaled_observed(
    workflow: &CompiledWorkflow,
    capabilities: &Capabilities,
    checkpointer: Arc<dyn Checkpointer<Value>>,
    thread_id: &str,
    journal: Arc<dyn GraphEventJournal>,
    observer: &Arc<dyn RunObserver>,
) -> Result<JournaledRunOutcome> {
    let (outcome, graph_run_ids) = resume_with_checkpointer_inner(
        workflow,
        capabilities,
        checkpointer,
        thread_id,
        Continuation::Retry,
        Some(journal),
        observer,
    )
    .await?;
    Ok(JournaledRunOutcome {
        outcome,
        graph_run_ids,
    })
}

/// Like [`retry_with_checkpointer`], but reports live progress to `observer`.
///
/// The observer sees `on_step_finish` for every node that runs *after* the
/// failure boundary — which is the point: a host watching a continued run
/// should see the work that is actually happening, not a replay of the prefix
/// that is not.
///
/// # Errors
/// Same as [`retry_with_checkpointer`].
pub async fn retry_with_checkpointer_observed(
    workflow: &CompiledWorkflow,
    capabilities: &Capabilities,
    checkpointer: Arc<dyn Checkpointer<Value>>,
    thread_id: &str,
    observer: &Arc<dyn RunObserver>,
) -> Result<RunOutcome> {
    let (outcome, _run_ids) = resume_with_checkpointer_inner(
        workflow,
        capabilities,
        checkpointer,
        thread_id,
        Continuation::Retry,
        None,
        observer,
    )
    .await?;
    Ok(outcome)
}
