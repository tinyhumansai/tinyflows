use super::*;

/// The default thread id for a run: the workflow's trigger (entry) node id.
///
/// Used by the non-injectable entry points ([`run`], [`run_with_observer`],
/// [`run_resumable`]) so a run is keyed under a stable, workflow-derived id —
/// preserving the pre-injectable-checkpointer behavior exactly.
///
/// # Errors
/// Returns [`EngineError::Validation`] if the workflow has no trigger node.
pub(super) fn default_thread_id(workflow: &CompiledWorkflow) -> Result<String> {
    Ok(workflow
        .graph
        .trigger()
        .ok_or(EngineError::Validation(ValidationError::MissingTrigger))?
        .id
        .clone())
}

/// Cancels background tasks that a cancelled run started.
async fn cancel_spawned_tasks(
    workflow: &CompiledWorkflow,
    state: &Value,
    capabilities: &Capabilities,
) {
    let Some(runner) = capabilities.tasks.as_ref() else {
        return;
    };
    for spawn in workflow
        .graph
        .nodes
        .iter()
        .filter(|node| node.kind == NodeKind::Spawn)
    {
        let Some(slot) = state.get("nodes").and_then(|nodes| nodes.get(&spawn.id)) else {
            continue;
        };
        let mut item_lists: Vec<&Vec<Value>> = slot
            .get("items")
            .and_then(Value::as_array)
            .into_iter()
            .collect();
        item_lists.extend(
            slot.get("lanes")
                .and_then(Value::as_object)
                .into_iter()
                .flat_map(|lanes| lanes.values())
                .filter_map(|lane| lane.get("items").and_then(Value::as_array)),
        );
        for ticket in item_lists.into_iter().flatten().filter_map(|item| {
            item.get("json")
                .and_then(|json| json.get("ticket"))
                .and_then(Value::as_str)
        }) {
            if let Err(error) = runner.cancel(ticket).await {
                tracing::warn!(%ticket, %error, "failed to cancel spawned task");
            }
        }
    }
}

/// Builds the graph-runtime graph for `workflow` under the supplied
/// `checkpointer`, drives the first run keyed under `thread_id`, and returns the
/// still-live compiled graph, that `thread_id`, and the [`RunOutcome`].
///
/// Shared by [`run_with_observer`] / [`run_with_checkpointer`] — which discard
/// the graph — and [`run_resumable`], which keeps it (and thus its checkpointer)
/// alive so a later [`ResumableRun::resume`] can replay forward from the
/// persisted checkpoint without re-executing already-completed nodes.
///
/// # Errors
/// Same as [`run`].
pub(super) async fn build_and_run(
    workflow: &CompiledWorkflow,
    input: impl Into<RunInput>,
    capabilities: &Capabilities,
    observer: &Arc<dyn RunObserver>,
    config: RunConfig,
) -> Result<(CompiledGraph<Value, Value>, String, RunOutcome, GraphRunIds)> {
    let token = config.token.clone();
    let thread_id = config.thread_id.clone();
    let run_meta_overlay = config.run_meta_overlay.clone();
    // Declared inputs are resolved FIRST — before the run id is minted, before
    // the observer is told a run started, and before any graph is built. A
    // caller that gets an `Input` error can therefore be certain nothing ran and
    // nothing was observed, which is what lets a host reject a bad call without
    // recording a phantom run.
    let RunInput {
        trigger,
        inputs,
        approvals,
        run_id: host_run_id,
    } = input.into();
    let resolved_inputs = crate::model::resolve_inputs(&workflow.graph.inputs, &inputs)?;

    // Process-local, monotonic run id — no time/random source.
    let run_id = format!("run-{}", NEXT_RUN_ID.fetch_add(1, Ordering::Relaxed));
    observer.on_run_start(&run_id);

    // Node handlers stream finished steps here (they also fire
    // `on_step_finish`); the engine folds this into the final `Run`. A shared
    // `Mutex` is the simplest correct sink across the `'static + Send + Sync`
    // handler closures, which can't otherwise push to a common `Vec`.
    let steps: Arc<Mutex<Vec<ExecutionStep>>> = Arc::new(Mutex::new(Vec::new()));
    // Where a run-ending node failure leaves its structured error on the way
    // out (see the `stop`-policy branch in `build_graph`), so the caller gets
    // the real `EngineError` instead of its `Display` string.
    let terminal_error: Arc<Mutex<Option<EngineError>>> = Arc::new(Mutex::new(None));

    // Build and compile the graph under the host-supplied checkpointer;
    // `trigger_id` is the graph's entry node.
    let (compiled, trigger_id) = build_graph(
        workflow,
        capabilities,
        observer,
        &steps,
        &terminal_error,
        &config,
    )?;

    let seed_items = items_update(&trigger_id, &[Item::new(trigger.clone())], None)
        .map_err(|e| EngineError::Capability(e.to_string()))?;
    // `run.inputs` holds the resolved declared inputs — one entry per
    // declaration, defaults already applied. `expr_scope_for` lifts it to the
    // top-level `inputs` scope key, so node config addresses it as
    // `=inputs.<name>` (and jq programs walking `run` still see it too).
    let mut initial =
        json!({ "run": { "trigger": trigger, "inputs": resolved_inputs, "approvals": approvals } });
    // The host's durable run identity, seeded OUTSIDE `run.trigger` on purpose:
    // the trigger is caller-supplied and, on a webhook, attacker-influenced,
    // while this slot is one a host fills deliberately. Anything keyed on it
    // (today the `approval` node's `request_id`) therefore rests on an id the
    // host chose, not one a request could smuggle in. Absent when the caller
    // named no run, which is every caller predating `RunInput::run_id`.
    if let Some(host_run_id) = host_run_id {
        merge(&mut initial, json!({ "run": { "id": host_run_id } }));
    }
    merge(&mut initial, seed_items);
    // The nesting cap for `sub_workflow` chains, read off the trigger config
    // like every other run-level knob and seeded into the run state so the
    // `sub_workflow` node can read it back. Carried in run state rather than
    // consulted from the graph because a *child* run is compiled from the
    // child's own graph: without this the cap would silently revert to the
    // default one level down. `run_sub_workflow` forwards it to each child.
    if let Some(max_depth) = workflow
        .graph
        .trigger()
        .and_then(|t| t.config.get("max_sub_workflow_depth"))
        .and_then(Value::as_u64)
        .filter(|n| *n > 0)
    {
        merge(
            &mut initial,
            json!({ "run": { "max_sub_workflow_depth": max_depth } }),
        );
    }
    // The run's workspace: the directory an author-supplied `cwd` resolves
    // against and may not escape (see `crate::workdir`). Read off the trigger
    // config like every other run-level knob and seeded into the run state, so
    // a node reads it back without knowing anything about the graph. A host that
    // pins a workspace per run rather than per graph puts a `workspace` key on
    // the trigger payload instead; both are read by `workdir::run_workspace`.
    if let Some(workspace) = workflow
        .graph
        .trigger()
        .and_then(|t| t.config.get("workspace"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|w| !w.is_empty())
    {
        merge(&mut initial, json!({ "run": { "workspace": workspace } }));
    }
    // Optional run-level metadata overlaid onto `run` before the run starts —
    // e.g. the `sub_workflow_depth` counter a nested `sub_workflow` run threads
    // to bound recursion (see [`run_sub_workflow`]). Merged (not overwritten) so
    // it sits alongside `run.trigger`.
    if let Some(overlay) = run_meta_overlay {
        merge(&mut initial, json!({ "run": overlay }));
    }

    // The run is keyed under the caller-supplied `thread_id` (the default paths
    // pass the trigger id, preserving prior behavior); this is where the
    // checkpointer persists the interrupt boundary.
    let execution = match compiled.run_with_thread(thread_id.clone(), initial).await {
        Ok(execution) => execution,
        Err(e) => {
            // A `stop`-policy node failure (or any driver error) surfaces here as
            // `Err`. Before propagating it, build a terminal `Failed` run record
            // (with whatever steps were collected) and fire `on_run_finish`, so an
            // observer that saw `on_run_start` is not left with a run that appears
            // to be running forever. This is the single choke point shared by every
            // observed entry point, so the failure lifecycle fires for all of them.
            let run_record = Run {
                id: run_id,
                status: RunStatus::Failed,
                steps: steps.lock().expect("steps mutex poisoned").clone(),
            };
            observer.on_run_finish(&run_record);
            // A node that ended the run stashed its real error on the way out;
            // prefer it so callers can match on the variant (e.g.
            // `EngineError::LoopLimit`). Falling back to the stringified driver
            // error covers failures that came from the graph runtime itself — a hit
            // recursion limit, a checkpointer fault — which have no node error
            // behind them.
            let structured = terminal_error
                .lock()
                .expect("terminal error mutex poisoned")
                .take();
            return Err(structured.unwrap_or_else(|| EngineError::Capability(e.to_string())));
        }
    };

    // Gates that paused the run awaiting approval, surfaced from the interrupts
    // the runtime returned at the boundary.
    //
    // Keyed by the interrupt's `id`, not by the node that raised it. For an
    // ordinary `requires_approval` gate the two are the same string. They differ
    // where one node speaks for gates that are not its own — a `sub_workflow`
    // node reports its child's gates as `<node>::<child gate>`, because parent
    // and child are separate graphs whose ids would otherwise collide.
    let pending_approvals: Vec<String> = execution
        .interrupts
        .iter()
        .map(|interrupt| interrupt.id.clone())
        .collect();

    if token.is_cancelled() {
        cancel_spawned_tasks(workflow, &execution.state, capabilities).await;
    }

    tracing::info!(
        steps = execution.steps,
        visited = execution.visited.len(),
        pending_approvals = pending_approvals.len(),
        "workflow run finished"
    );

    // Reaching here means the run settled without a `stop`-policy failure
    // (those bubble out as `Err` above). A node handled by `continue`/`route`
    // still records a per-step `Error` while the run proceeds, so the terminal
    // status is derived from the steps rather than assumed clean: any error step
    // makes this `CompletedWithErrors`, so a host that reads only `status` still
    // learns a node failed instead of seeing an unqualified success (#661 L1).
    let collected_steps = steps.lock().expect("steps mutex poisoned").clone();
    let status = if collected_steps
        .iter()
        .any(|step| matches!(step.status, StepStatus::Error))
    {
        RunStatus::CompletedWithErrors
    } else {
        RunStatus::Completed
    };
    let run_record = Run {
        id: run_id,
        status,
        steps: collected_steps,
    };
    observer.on_run_finish(&run_record);

    // The runtime-minted run ids: a journal (when attached) keys this run's
    // observations by `run_id`, so surface both ids to the caller.
    let graph_run_ids = GraphRunIds {
        run_id: execution.run_id.as_str().to_string(),
        root_run_id: execution.root_run_id.as_str().to_string(),
    };

    Ok((
        compiled,
        thread_id,
        RunOutcome {
            output: execution.state,
            pending_approvals,
            // A cancelled token means at least one node boundary short-circuited,
            // so surface the run as cancelled (its `output` is partial).
            cancelled: token.is_cancelled(),
        },
        graph_run_ids,
    ))
}

/// Resumes a paused run by re-running `workflow` with `newly_approved` node ids
/// added to the run's approvals, so previously-gated nodes now execute.
///
/// This is the approve-and-continue completion of the human-in-the-loop loop:
/// a run that paused with `RunOutcome::pending_approvals`
/// is continued by supplying the approvals. It re-executes the workflow with the
/// merged approval set (deterministic; checkpointed super-step replay is a future
/// optimization). Prior approvals in `input["approvals"]` are preserved and unioned.
///
/// # Errors
/// Same as [`run`]: returns an [`EngineError`] if lowering, compilation, or
/// execution of the resumed run fails.
pub async fn resume(
    workflow: &CompiledWorkflow,
    input: impl Into<RunInput>,
    newly_approved: Vec<String>,
    capabilities: &Capabilities,
) -> Result<RunOutcome> {
    run(
        workflow,
        merge_approvals(input, newly_approved),
        capabilities,
    )
    .await
}

/// Unions `newly_approved` into the run input's `trigger["approvals"]`, leaving
/// the declared-input values and the run's host id untouched.
///
/// Reads defensively: a missing or non-array `approvals` yields an empty
/// starting set, and a non-object trigger (which carries no fields to preserve)
/// is replaced by a fresh object holding just the merged approvals. Declared
/// inputs ride along unchanged — a resume re-runs the *same* parameterized
/// workflow, so dropping them would silently change what it does.
///
/// The returned [`RunInput::approvals`] carries **only** what a host explicitly
/// authorised — never an id read out of the trigger payload. See the body for
/// why the two sets are kept apart.
pub(super) fn merge_approvals(input: impl Into<RunInput>, newly_approved: Vec<String>) -> RunInput {
    let RunInput {
        mut trigger,
        inputs,
        approvals: prior,
        run_id,
    } = input.into();

    // Approval **provenance** is preserved rather than flattened, and the two
    // sets are deliberately not the same list.
    //
    // `explicit` is what a host actually authorised: the ids passed to
    // `engine::resume`, plus any carried on `RunInput::approvals`. It never
    // includes anything read out of the trigger, because the trigger is the
    // payload a caller submitted — on a webhook, attacker-supplied. Flattening
    // the two meant a caller-written `trigger.approvals` entry was promoted
    // into the trusted list by the first resume, so a self-approval that the
    // initial run correctly refused went through on the next one.
    //
    // `trigger.approvals` still receives the union, unchanged, because the
    // `requires_approval` gate in `engine::build::activation` reads exactly
    // that key and callers are documented as being able to write approvals
    // there directly. Narrowing *that* is a separate change to a shared,
    // documented channel; this only stops trigger-origin ids laundering
    // themselves into the explicit one.
    let mut explicit: Vec<String> = Vec::new();
    for id in newly_approved.into_iter().chain(prior) {
        if !explicit.contains(&id) {
            explicit.push(id);
        }
    }

    // The union written back to the trigger: whatever the caller already had
    // there, plus everything explicitly authorised.
    let mut union: Vec<String> = trigger
        .get("approvals")
        .and_then(Value::as_array)
        .map(|existing| {
            existing
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    for id in &explicit {
        if !union.contains(id) {
            union.push(id.clone());
        }
    }

    if let Value::Object(map) = &mut trigger {
        map.insert("approvals".to_string(), json!(union));
    } else {
        trigger = json!({ "approvals": union });
    }

    RunInput {
        trigger,
        inputs,
        approvals: explicit,
        // Carried through untouched. A resume is the SAME run continuing, so
        // dropping its id here would re-key everything derived from it — a
        // paused human review would open a second card instead of resolving
        // the one already in front of somebody.
        run_id,
    }
}
