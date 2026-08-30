use super::super::*;
use crate::graph::error::{GraphError, Result};

impl<State, Update> CompiledGraph<State, Update>
where
    State: Clone + Send + Sync + 'static,
    Update: Send + 'static,
{
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn execute_run(
        &self,
        run_id: RunId,
        mut state: State,
        initial_active: Vec<Activation>,
        thread_id: Option<ThreadId>,
        mut resume_map: HashMap<NodeId, serde_json::Value>,
        initial_barriers: HashMap<NodeId, HashSet<NodeId>>,
        initial_parent: Option<String>,
    ) -> Result<GraphExecution<State>> {
        let started_at = SystemTime::now();
        let mut visited: Vec<NodeId> = Vec::new();
        let mut steps = 0usize;
        let mut last_checkpoint: Option<CheckpointId> = None;
        // On resume this is the loaded checkpoint's id, so the first boundary
        // checkpoint after a resume chains onto pre-interrupt history rather
        // than orphaning the lineage (which would stop `get_state_history` at
        // the resume point and let `prune` delete the ancestors).
        let mut parent_checkpoint: Option<String> = initial_parent;

        // Build this run's recursion stack from the inherited parent frames and
        // push the frame for this graph call. A push that would exceed
        // `max_depth` fails the run with a clear recursion error before any
        // node executes. Graph-call depth (the stack) is tracked separately
        // from node-loop visits (`node_visits`, below).
        let mut recursion =
            RecursionStack::with_frames(self.recursion_frames.clone(), self.recursion_policy);
        // Run lineage: the root is the first inherited frame's run (the top of
        // the recursion tree) or this run when top-level; the parent is the
        // enclosing run, if any.
        let root_run_id = self
            .recursion_frames
            .first()
            .map(|f| f.run_id.clone())
            .unwrap_or_else(|| run_id.clone());
        let parent_run_id = self.recursion_frames.last().map(|f| f.run_id.clone());
        let this_frame = RecursionFrame {
            graph_id: self.graph_id.clone(),
            node_id: self.recursion_node.clone(),
            run_id: run_id.clone(),
            task_id: None,
            namespace: self.namespace.clone(),
            depth: recursion.depth(),
            parent: parent_run_id.clone(),
        };
        if let Err(err) = recursion.push(this_frame) {
            self.emit(GraphEvent::RunStarted {
                run_id: run_id.clone(),
            });
            self.fail_run(&run_id, &thread_id, started_at, steps, &err, None)
                .await;
            return Err(err);
        }
        // Serialized once per run for embedding in every checkpoint's metadata.
        let recursion_meta =
            serde_json::to_value(recursion.frames()).unwrap_or(serde_json::Value::Null);
        // The live frame stack handed to node contexts so a subgraph node can
        // seed an embedded child with this run's recursion path, plus the
        // per-run sink the node reports its spawned child run into.
        let live_frames = recursion.frames().to_vec();
        let child_sink = ChildRunSink::new();
        // Accumulates every child run spawned across all supersteps for the
        // final `GraphExecution::child_runs`.
        let mut all_child_runs: Vec<ChildRun> = Vec::new();
        // Per-node activation counts for `max_visits_per_node` enforcement.
        let mut node_visits: HashMap<NodeId, usize> = HashMap::new();
        let mut active = initial_active;
        // Barrier/waiting-edge arrivals accumulate across supersteps: a waiting
        // node only activates once every required predecessor has arrived.
        // Seeded from the resumed checkpoint so a join's precondition survives
        // an interrupt/failure boundary.
        let mut barrier_arrivals: HashMap<NodeId, HashSet<NodeId>> = initial_barriers;
        // Under `DurabilityMode::Async`, boundary checkpoint writes run on
        // spawned background tasks tracked here. Failures are surfaced at the
        // next durability boundary; every terminal path drains the tracker so
        // the run result reflects persistence failures (see
        // `AsyncCheckpointWrites`).
        let mut async_writes = AsyncCheckpointWrites::default();

        self.emit(GraphEvent::RunStarted {
            run_id: run_id.clone(),
        });
        // Surface this run's recursion depth so observers can attribute nested
        // runs without reconstructing the tree from logs.
        self.emit(GraphEvent::RecursionDepthChanged {
            depth: recursion.depth(),
        });
        // Record the run as live before the first superstep is scheduled.
        let mut running = self.base_status(&run_id, &thread_id, started_at);
        running.active_nodes = activation_nodes(&active);
        self.save_status(running).await;

        while !active.is_empty() {
            // The effective step cap is the smaller of the builder's recursion
            // limit and the policy's `max_total_steps`, so a policy never
            // loosens an existing limit. Both surface a `RecursionLimit`.
            let step_limit = self
                .recursion_limit
                .min(self.recursion_policy.max_total_steps);
            if steps >= step_limit {
                let err = GraphError::RecursionLimit(step_limit);
                return self
                    .fail_and_return(
                        &run_id,
                        &thread_id,
                        started_at,
                        steps,
                        &mut async_writes,
                        err,
                    )
                    .await;
            }
            // Whole-run wall-clock deadline: stop *between* super-steps once the
            // elapsed run time reaches it, leaving the last committed boundary
            // checkpoint intact (unlike an external `tokio::time::timeout`, which
            // aborts mid-super-step and cannot). The already-completed super-steps
            // and their checkpoints are preserved; the run fails with `Timeout`.
            if let Some(deadline) = self.run_deadline {
                let elapsed = started_at.elapsed().unwrap_or_default();
                if elapsed >= deadline {
                    let err = GraphError::Timeout(format!(
                        "graph run exceeded its {deadline:?} deadline after {steps} super-step(s) \
                         ({elapsed:?} elapsed)"
                    ));
                    return self
                        .fail_and_return(
                            &run_id,
                            &thread_id,
                            started_at,
                            steps,
                            &mut async_writes,
                            err,
                        )
                        .await;
                }
            }
            // Node-loop recursion: enforce `max_visits_per_node` per activation.
            for activation in &active {
                if let Err(err) = recursion.record_node_visit(&mut node_visits, &activation.node) {
                    return self
                        .fail_and_return(
                            &run_id,
                            &thread_id,
                            started_at,
                            steps,
                            &mut async_writes,
                            err,
                        )
                        .await;
                }
            }
            steps += 1;
            // Assign identities before any branch runs. A failure checkpoint
            // carries these identities with its pending activations, letting a
            // later resume skip only the completed fan-out task.
            for (index, activation) in active.iter_mut().enumerate() {
                if activation.task_id.is_empty() {
                    activation.task_id = format!("{steps}:{index}:{}", activation.node);
                }
            }
            self.emit(GraphEvent::StepStarted {
                step: steps,
                active: activation_nodes(&active),
            });

            let run_result = if self.parallel && active.len() > 1 {
                self.run_active_parallel(
                    &active,
                    &state,
                    &run_id,
                    &thread_id,
                    steps,
                    &mut resume_map,
                    &mut visited,
                    &root_run_id,
                    &live_frames,
                    &child_sink,
                )
                .await
            } else {
                self.run_active_sequential(
                    &active,
                    &state,
                    &run_id,
                    &thread_id,
                    steps,
                    &mut resume_map,
                    &mut visited,
                    &root_run_id,
                    &live_frames,
                    &child_sink,
                )
                .await
            };
            let StepRun {
                updates,
                goto_map,
                completed: completed_indices,
                interrupts,
                failure,
            } = match run_result {
                Ok(step_run) => step_run,
                Err(err) => {
                    return self
                        .fail_and_return(
                            &run_id,
                            &thread_id,
                            started_at,
                            steps,
                            &mut async_writes,
                            err,
                        )
                        .await;
                }
            };

            // Apply collected updates through the reducer at the boundary. A
            // reducer error here must still fail the run (not just unwind
            // leaving it `Running`).
            for update in updates {
                state = match self.reducer.apply(state, update) {
                    Ok(state) => state,
                    Err(err) => {
                        return self
                            .fail_and_return(
                                &run_id,
                                &thread_id,
                                started_at,
                                steps,
                                &mut async_writes,
                                err,
                            )
                            .await;
                    }
                };
            }

            // Collect any child runs spawned by subgraph nodes this step. They
            // are embedded into this boundary's checkpoint metadata (keyed by
            // node) and accumulated onto the final `GraphExecution`.
            let step_child_runs = child_sink.drain();
            all_child_runs.extend(step_child_runs.iter().cloned());
            let child_runs_meta =
                serde_json::to_value(&step_child_runs).unwrap_or(serde_json::Value::Null);

            // Node-handler failure (survived any node-retry policy): the updates
            // of the branches that completed before it are already folded into
            // `state` above, so persist a resumable failure-boundary checkpoint
            // scheduling the failed node (and the not-yet-run tail) for a later
            // `resume`/`retry`, record a `Failed` status carrying the error and
            // that checkpoint, and abort. Without a checkpointer/thread the
            // checkpoint is a no-op and the run aborts exactly as before.
            if let Some(fail) = failure {
                let StepFailure {
                    failed_index,
                    error,
                } = fail;
                let failed_node = active[failed_index].node.clone();
                // Schedule the successors of the branches that completed before
                // the failure (they succeeded; their routing must not be lost)
                // followed by the failed branch and the not-yet-run tail, which
                // re-run on resume with their `Send` args preserved.
                let successors = match self.route_completed(
                    &active[..failed_index],
                    &goto_map,
                    &state,
                    &mut barrier_arrivals,
                ) {
                    Ok(successors) => successors,
                    Err(route_err) => {
                        return self
                            .fail_and_return(
                                &run_id,
                                &thread_id,
                                started_at,
                                steps,
                                &mut async_writes,
                                route_err,
                            )
                            .await;
                    }
                };
                let mut pending = successors;
                pending.extend(active[failed_index..].iter().cloned());
                // Settle any in-flight Async background writes before the
                // failure-boundary persist so earlier boundaries are durable
                // when the run aborts. Like the persist error below, a
                // background write error must not replace the original node
                // error, so it is intentionally dropped here.
                let _ = async_writes.drain().await;
                // A failure-boundary persist error must not replace the original
                // node error: keep reporting the node error and just drop the
                // resumable checkpoint reference.
                let checkpoint_id = self
                    .persist_failure_checkpoint(
                        &thread_id,
                        &run_id,
                        &state,
                        &pending,
                        &active[..failed_index],
                        &barrier_arrivals,
                        parent_checkpoint.clone(),
                        steps,
                        &failed_node,
                        &error,
                        &recursion_meta,
                        &child_runs_meta,
                    )
                    .await
                    .unwrap_or(None);
                self.fail_run(
                    &run_id,
                    &thread_id,
                    started_at,
                    steps,
                    &error,
                    checkpoint_id,
                )
                .await;
                return Err(error);
            }

            if !interrupts.is_empty() {
                return self
                    .finish_interrupt(
                        &run_id,
                        &thread_id,
                        started_at,
                        steps,
                        &mut async_writes,
                        &active,
                        &goto_map,
                        &completed_indices,
                        &interrupts,
                        state,
                        &mut barrier_arrivals,
                        parent_checkpoint,
                        &recursion_meta,
                        &child_runs_meta,
                        &root_run_id,
                        &parent_run_id,
                        all_child_runs,
                        visited,
                    )
                    .await;
            }

            // Select the next active set from commands or static/conditional
            // edges, evaluated against the freshly-committed state. Barrier
            // arrivals accumulate into `barrier_arrivals` (persisted below).
            let next = match self.route_completed(&active, &goto_map, &state, &mut barrier_arrivals)
            {
                Ok(next) => next,
                Err(route_err) => {
                    return self
                        .fail_and_return(
                            &run_id,
                            &thread_id,
                            started_at,
                            steps,
                            &mut async_writes,
                            route_err,
                        )
                        .await;
                }
            };

            // Persist a boundary checkpoint. Under `Exit` durability only the
            // terminal boundary (the step that empties the active set) is
            // written; `Sync`/`Async` persist every boundary. `Async` hands
            // non-terminal writes to background tasks instead of awaiting them
            // inline.
            let persist_now = match self.durability {
                DurabilityMode::Exit => next.is_empty(),
                DurabilityMode::Sync | DurabilityMode::Async => true,
            };
            // Async durability: surface any background write failure recorded
            // since the previous boundary. The run fails at the first
            // durability boundary that observes the loss rather than silently
            // continuing with a hole in its lineage.
            if let Some(err) = async_writes.take_failure().await {
                return self
                    .fail_and_return(
                        &run_id,
                        &thread_id,
                        started_at,
                        steps,
                        &mut async_writes,
                        err,
                    )
                    .await;
            }
            let terminal = next.is_empty();
            let checkpoint_id = if persist_now {
                let persisted = if matches!(self.durability, DurabilityMode::Async) && !terminal {
                    self.persist_checkpoint_nonblocking(
                        &mut async_writes,
                        &thread_id,
                        &run_id,
                        &state,
                        &next,
                        &active,
                        &barrier_arrivals,
                        parent_checkpoint.clone(),
                        steps,
                        &recursion_meta,
                        &child_runs_meta,
                    )
                    .await
                } else {
                    // Terminal boundary: drain every in-flight background
                    // write first (the "final await at run end"), so a lost
                    // Async checkpoint fails the run instead of being
                    // swallowed. The final checkpoint itself is then written
                    // synchronously in every mode.
                    if terminal && let Err(err) = async_writes.drain().await {
                        return self
                            .fail_and_return(
                                &run_id,
                                &thread_id,
                                started_at,
                                steps,
                                &mut async_writes,
                                err,
                            )
                            .await;
                    }
                    self.persist_checkpoint(
                        &thread_id,
                        &run_id,
                        &state,
                        &next,
                        &active,
                        Vec::new(),
                        &[],
                        &barrier_arrivals,
                        parent_checkpoint.clone(),
                        steps,
                        "loop",
                        &recursion_meta,
                        &child_runs_meta,
                    )
                    .await
                };
                match persisted {
                    Ok(id) => id,
                    Err(persist_err) => {
                        return self
                            .fail_and_return(
                                &run_id,
                                &thread_id,
                                started_at,
                                steps,
                                &mut async_writes,
                                persist_err,
                            )
                            .await;
                    }
                }
            } else {
                None
            };
            if let Some(id) = &checkpoint_id {
                last_checkpoint = Some(id.clone());
                parent_checkpoint = Some(id.to_string());
            }

            self.emit(GraphEvent::StepCompleted { step: steps });
            active = next;
        }

        self.finish_completed(
            state,
            run_id,
            thread_id,
            started_at,
            steps,
            last_checkpoint,
            root_run_id,
            parent_run_id,
            all_child_runs,
            visited,
        )
        .await
    }
}
