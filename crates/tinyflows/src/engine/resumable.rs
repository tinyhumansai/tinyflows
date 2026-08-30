use super::*;

/// Like [`resume`], but observes `token`: cancelling it winds the resumed run
/// down at the next node boundary and sets [`RunOutcome::cancelled`]. This is the
/// re-run-based resume (the same deterministic replay [`resume`] performs), made
/// cooperatively cancellable.
///
/// # Errors
/// Same as [`resume`].
pub async fn resume_cancellable(
    workflow: &CompiledWorkflow,
    input: impl Into<RunInput>,
    newly_approved: Vec<String>,
    capabilities: &Capabilities,
    token: CancellationToken,
) -> Result<RunOutcome> {
    run_cancellable(
        workflow,
        merge_approvals(input, newly_approved),
        capabilities,
        token,
    )
    .await
}

/// A live, resumable workflow run.
///
/// Unlike the re-run-based [`resume`], this keeps the compiled graph-runtime graph
/// (and therefore its checkpointer) alive after the initial run, so
/// [`ResumableRun::resume`] can continue **from the persisted checkpoint** —
/// the runtime replays forward from the interrupt boundary, so nodes that already
/// completed are **not** re-executed.
pub struct ResumableRun {
    /// The compiled graph that ran, kept alive so its in-memory checkpointer
    /// still holds the interrupt boundary a resume replays from.
    graph: CompiledGraph<Value, Value>,
    /// The thread id the initial run (and every resume) is keyed under.
    thread_id: String,
    /// The outcome of the initial run, before any resume.
    outcome: RunOutcome,
}

/// Like [`run_cancellable_with_observer`], but routes every node activation
/// through `interceptor` before and after it executes.
///
/// This is the engine's only execution-**gating** entry point. A
/// [`RunObserver`] watches a run; a
/// [`StepInterceptor`](crate::interception::StepInterceptor) can change one —
/// substituting a node's output, injecting a failure, patching the state it
/// reads, or parking the activation while something else decides. That is what
/// [`crate::testkit`]'s breakpoints are built on, and a host can implement the
/// trait directly for a fault-injection harness of its own.
///
/// The returned [`ResumableRun`] keeps the compiled graph and its checkpointer
/// alive, so a finished debug session can still be inspected and replayed.
///
/// An interceptor that returns
/// [`StepAction::Continue`](crate::interception::StepAction::Continue) with no
/// patch leaves the run byte-identical to [`run`].
///
/// # Errors
/// Same as [`run`].
pub async fn run_intercepted(
    workflow: &CompiledWorkflow,
    input: impl Into<RunInput>,
    capabilities: &Capabilities,
    observer: &Arc<dyn RunObserver>,
    token: CancellationToken,
    interceptor: Arc<dyn StepInterceptor>,
) -> Result<(RunOutcome, ResumableRun)> {
    let (graph, thread_id, outcome, _run_ids) = build_and_run(
        workflow,
        input,
        capabilities,
        observer,
        RunConfig::new(workflow)?
            .with_token(token)
            .with_interceptor(interceptor),
    )
    .await?;
    Ok((
        outcome.clone(),
        ResumableRun {
            graph,
            thread_id,
            outcome,
        },
    ))
}

impl ResumableRun {
    /// The outcome of the initial run, before any [`resume`](ResumableRun::resume).
    /// Its [`RunOutcome::pending_approvals`] lists the gate nodes awaiting
    /// approval.
    pub fn outcome(&self) -> &RunOutcome {
        &self.outcome
    }

    /// Resumes the run from its checkpoint, approving the currently-interrupted
    /// gate node(s) so the workflow proceeds. `newly_approved` are the gate ids
    /// being approved; they are also recorded into the run's approvals for
    /// downstream visibility.
    ///
    /// the runtime replays forward from the persisted checkpoint — the interrupted
    /// gate re-runs (now approved, because the resume value reaches it via
    /// `NodeContext::resume`) and its downstream continues, while nodes that
    /// already completed are not re-executed.
    ///
    /// # Errors
    /// Returns [`EngineError::Capability`] if the checkpointed resume fails (for
    /// example, when there is no pending checkpoint to resume from).
    pub async fn resume(&self, newly_approved: Vec<String>) -> Result<RunOutcome> {
        let approvals_update = json!({
            "run": { "trigger": { "approvals": newly_approved.clone() } }
        });
        // Deliver the explicit `approved` gate id list as the resume value.
        // The runtime ignores the `with_update` state write on resume, so the
        // resume value is the sole approval channel: each interrupted gate
        // proceeds only if its id is listed, leaving any other parallel gate
        // pending rather than blanket-approving every interrupt with a bare `true`.
        let execution = self
            .graph
            .resume(
                self.thread_id.as_str(),
                Command::resume(json!({ "approved": newly_approved }))
                    .with_update(approvals_update),
            )
            .await
            .map_err(|e| EngineError::Capability(e.to_string()))?;

        let pending_approvals: Vec<String> = execution
            .interrupts
            .iter()
            .map(|interrupt| interrupt.id.clone())
            .collect();

        Ok(RunOutcome {
            output: execution.state,
            pending_approvals,
            cancelled: false,
        })
    }
}

/// Runs `workflow` like [`run`], but returns a [`ResumableRun`] whose compiled
/// graph (and checkpointer) is kept alive so [`ResumableRun::resume`] can
/// continue from the persisted checkpoint without re-executing completed nodes.
///
/// A no-op [`RunObserver`] is installed; all execution behavior is identical to
/// [`run`].
///
/// # Errors
/// Same as [`run`].
pub async fn run_resumable(
    workflow: &CompiledWorkflow,
    input: impl Into<RunInput>,
    capabilities: &Capabilities,
) -> Result<ResumableRun> {
    let observer = Arc::new(crate::observability::NoopObserver) as Arc<dyn RunObserver>;
    // Default (non-injectable) path: a process-local in-memory checkpointer,
    // kept alive on the returned `ResumableRun`, keyed by the trigger id.
    let (graph, thread_id, outcome, _run_ids) = build_and_run(
        workflow,
        input,
        capabilities,
        &observer,
        RunConfig::new(workflow)?,
    )
    .await?;
    Ok(ResumableRun {
        graph,
        thread_id,
        outcome,
    })
}

/// Runs `workflow` under a **host-injected** `checkpointer`, keying the run's
/// persisted state by the caller-supplied `thread_id`.
///
/// This is the durable, cross-process entry point. Unlike [`run`] — which uses a
/// process-local [`InMemoryCheckpointer`] keyed by the trigger id — this drives
/// the run under whatever [`Checkpointer`] the host supplies (for example a
/// database-backed run ledger), keyed by a stable `thread_id` the host chooses.
/// When the run pauses at a human-in-the-loop approval gate, its interrupt
/// boundary is persisted into the host's checkpointer under `thread_id`; the
/// returned [`RunOutcome::pending_approvals`] lists the gate node ids awaiting
/// approval, and their downstream did not run.
///
/// A host can then continue the run later — even after a process restart — by
/// rebuilding its [`Capabilities`] and the same checkpointer and calling
/// [`resume_with_checkpointer`] with the same `thread_id`.
///
/// A no-op [`RunObserver`] is installed; all execution behavior (retry,
/// `on_error`, HITL interrupts, conditional routing, tracing) is identical to
/// [`run`].
///
/// # Errors
/// Same as [`run`]: returns an [`EngineError`] if lowering, compilation, or
/// execution (including any node executor error) fails.
pub async fn run_with_checkpointer(
    workflow: &CompiledWorkflow,
    input: impl Into<RunInput>,
    capabilities: &Capabilities,
    checkpointer: Arc<dyn Checkpointer<Value>>,
    thread_id: &str,
) -> Result<RunOutcome> {
    let observer = Arc::new(crate::observability::NoopObserver) as Arc<dyn RunObserver>;
    let (_graph, _thread_id, outcome, _run_ids) = build_and_run(
        workflow,
        input,
        capabilities,
        &observer,
        RunConfig::new(workflow)?.with_checkpointer(checkpointer, thread_id),
    )
    .await?;
    Ok(outcome)
}

/// Resumes a run that was previously started with [`run_with_checkpointer`],
/// continuing it **from the persisted checkpoint** in the host-injected
/// `checkpointer`.
///
/// This is the durable, cross-process resume path. It rebuilds the identical
/// graph-runtime graph for `workflow`, re-attaches the **same** `checkpointer`,
/// and resumes the persisted `thread_id` — so a host can run, persist to its
/// own durable store, and later (even after a full process restart) reconstruct
/// its [`Capabilities`] plus checkpointer and pick the run back up by
/// `thread_id`. Nodes that already completed before the pause are not
/// re-executed; the runtime replays forward from the interrupt boundary.
///
/// `newly_approved` are the gate node ids being approved. Approval flows through
/// the same mechanism [`ResumableRun::resume`] uses: [`Command::resume`]
/// delivers a resume value that reaches the interrupted gate via
/// `NodeContext::resume`, which the gate treats as approval. The ids are also
/// recorded into the run's approvals for downstream visibility. (Note: in
/// the runtime the accompanying state update is ignored on resume, so the resume
/// value itself is the operative approval channel.)
///
/// Returns a fresh [`RunOutcome`]: `output` is the resumed run's final state and
/// `pending_approvals` lists any gate still awaiting approval (empty once the
/// run completes).
///
/// # Errors
/// Returns [`EngineError`] if rebuilding/compiling the graph fails, or
/// [`EngineError::Capability`] if the checkpointed resume fails — for example
/// when the `checkpointer` holds no pending checkpoint for `thread_id`.
pub async fn resume_with_checkpointer(
    workflow: &CompiledWorkflow,
    capabilities: &Capabilities,
    checkpointer: Arc<dyn Checkpointer<Value>>,
    thread_id: &str,
    newly_approved: Vec<String>,
) -> Result<RunOutcome> {
    let observer = Arc::new(crate::observability::NoopObserver) as Arc<dyn RunObserver>;
    let (outcome, _run_ids) = resume_with_checkpointer_inner(
        workflow,
        capabilities,
        checkpointer,
        thread_id,
        Continuation::Approvals {
            approved: newly_approved,
            rejected: Vec::new(),
        },
        None,
        &observer,
    )
    .await?;
    Ok(outcome)
}

/// Like [`run_with_checkpointer`], but additionally attaches the host-supplied
/// `journal`: every graph event the run emits is recorded as a durable
/// [`GraphObservation`] keyed by the run's graph run id, which is
/// returned on the [`JournaledRunOutcome`] so the host can read the exact
/// slice back (`journal.read_from(&graph_run_ids.run_id, 0)`) — for example to
/// export the run to Langfuse after it settles.
///
/// All execution behavior is identical to [`run_with_checkpointer`]; the
/// journal sits off the hot path (appends are best-effort inside the runtime)
/// and never fails the run.
///
/// # Errors
/// Same as [`run_with_checkpointer`].
pub async fn run_with_checkpointer_journaled(
    workflow: &CompiledWorkflow,
    input: impl Into<RunInput>,
    capabilities: &Capabilities,
    checkpointer: Arc<dyn Checkpointer<Value>>,
    thread_id: &str,
    journal: Arc<dyn GraphEventJournal>,
) -> Result<JournaledRunOutcome> {
    let observer = Arc::new(crate::observability::NoopObserver) as Arc<dyn RunObserver>;
    run_with_checkpointer_journaled_observed(
        workflow,
        input,
        capabilities,
        checkpointer,
        thread_id,
        journal,
        &observer,
    )
    .await
}

/// Like [`run_with_checkpointer_journaled`], but additionally reports live
/// run/step records to the host-supplied `observer` as the run executes
/// ([`RunObserver::on_run_start`] once, [`RunObserver::on_step_finish`] per
/// non-trigger node as it finishes, [`RunObserver::on_run_finish`] once at
/// settle). This is the durable + journaled + observed entry point a host uses
/// when it wants **both** post-run journal export **and** live per-step
/// observation (e.g. incremental run-history persistence and a progress feed).
///
/// The observer is held as `Arc<dyn RunObserver>` and cloned into each node
/// handler, which run across threads, so it must be cheap and non-blocking; see
/// [`RunObserver`]'s contract.
///
/// # Errors
/// Same as [`run_with_checkpointer_journaled`].
pub async fn run_with_checkpointer_journaled_observed(
    workflow: &CompiledWorkflow,
    input: impl Into<RunInput>,
    capabilities: &Capabilities,
    checkpointer: Arc<dyn Checkpointer<Value>>,
    thread_id: &str,
    journal: Arc<dyn GraphEventJournal>,
    observer: &Arc<dyn RunObserver>,
) -> Result<JournaledRunOutcome> {
    let (_graph, _thread_id, outcome, graph_run_ids) = build_and_run(
        workflow,
        input,
        capabilities,
        observer,
        RunConfig::new(workflow)?
            .with_checkpointer(checkpointer, thread_id)
            .with_journal(journal),
    )
    .await?;
    tracing::debug!(
        run_id = %graph_run_ids.run_id,
        root_run_id = %graph_run_ids.root_run_id,
        "journaled workflow run finished"
    );
    Ok(JournaledRunOutcome {
        outcome,
        graph_run_ids,
    })
}

/// Like [`resume_with_checkpointer`], but additionally attaches the
/// host-supplied `journal` to the resumed run (see
/// [`run_with_checkpointer_journaled`] for the journaling contract). The
/// resumed execution mints a **new** graph run id — returned on the
/// [`JournaledRunOutcome`] — so the host reads the resume's observations under
/// that id, not the original run's.
///
/// # Errors
/// Same as [`resume_with_checkpointer`].
pub async fn resume_with_checkpointer_journaled(
    workflow: &CompiledWorkflow,
    capabilities: &Capabilities,
    checkpointer: Arc<dyn Checkpointer<Value>>,
    thread_id: &str,
    newly_approved: Vec<String>,
    journal: Arc<dyn GraphEventJournal>,
) -> Result<JournaledRunOutcome> {
    let observer = Arc::new(crate::observability::NoopObserver) as Arc<dyn RunObserver>;
    resume_with_checkpointer_journaled_observed(
        workflow,
        capabilities,
        checkpointer,
        thread_id,
        newly_approved,
        Vec::new(),
        journal,
        &observer,
    )
    .await
}

/// Like [`resume_with_checkpointer_journaled`], but additionally reports live
/// step records to the host-supplied `observer` as the resumed run executes
/// (each node that runs after the interrupt boundary fires
/// [`RunObserver::on_step_finish`]). The durable + journaled + observed resume
/// counterpart to [`run_with_checkpointer_journaled_observed`].
///
/// `newly_approved` gate ids proceed on resume; `rejected` gate ids are **denied**
/// — each denied gate routes an error item to its `error` port (when one is
/// wired) or fails the run (when it has none). Pass an empty `rejected` for the
/// approve-only path; the two sets should be disjoint (a gate is approved or
/// denied, not both).
///
/// # Errors
/// Same as [`resume_with_checkpointer_journaled`].
#[allow(clippy::too_many_arguments)]
pub async fn resume_with_checkpointer_journaled_observed(
    workflow: &CompiledWorkflow,
    capabilities: &Capabilities,
    checkpointer: Arc<dyn Checkpointer<Value>>,
    thread_id: &str,
    newly_approved: Vec<String>,
    rejected: Vec<String>,
    journal: Arc<dyn GraphEventJournal>,
    observer: &Arc<dyn RunObserver>,
) -> Result<JournaledRunOutcome> {
    let (outcome, graph_run_ids) = resume_with_checkpointer_inner(
        workflow,
        capabilities,
        checkpointer,
        thread_id,
        Continuation::Approvals {
            approved: newly_approved,
            rejected,
        },
        Some(journal),
        observer,
    )
    .await?;
    tracing::debug!(
        run_id = %graph_run_ids.run_id,
        root_run_id = %graph_run_ids.root_run_id,
        "journaled workflow resume finished"
    );
    Ok(JournaledRunOutcome {
        outcome,
        graph_run_ids,
    })
}

/// Why a checkpointed thread is being continued.
///
/// The two boundaries a run can stop at need different things delivered back
/// into it, and nothing else about continuing differs — same graph rebuild,
/// same checkpointer, same fold. Keeping the difference in one value is what
/// lets the failure path reuse the whole of the approval path rather than
/// growing a parallel copy of it.
enum Continuation {
    /// The run paused at approval gates. Carries the operator's decisions.
    Approvals {
        /// Gates the operator allowed.
        approved: Vec<String>,
        /// Gates the operator refused.
        rejected: Vec<String>,
    },
    /// The run *failed*. Re-run the node that failed and the not-yet-run tail
    /// of its step, carrying no value — there is no decision to deliver, only
    /// work to redo.
    Retry,
}

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
