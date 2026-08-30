use super::super::*;
use crate::graph::error::{GraphError, Result};

impl<State, Update> CompiledGraph<State, Update>
where
    State: Clone + Send + Sync + 'static,
    Update: Send + 'static,
{
    /// Records and returns a successfully completed execution.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn finish_completed(
        &self,
        state: State,
        run_id: RunId,
        thread_id: Option<ThreadId>,
        started_at: SystemTime,
        steps: usize,
        last_checkpoint: Option<CheckpointId>,
        root_run_id: RunId,
        parent_run_id: Option<RunId>,
        all_child_runs: Vec<ChildRun>,
        visited: Vec<NodeId>,
    ) -> Result<GraphExecution<State>> {
        let mut status = self.base_status(&run_id, &thread_id, started_at);
        status.status = ExecutionStatus::Completed;
        status.current_step = steps;
        status.checkpoint_id = last_checkpoint.clone();
        status.ended_at = Some(SystemTime::now());
        self.save_status(status.clone()).await;
        self.emit(GraphEvent::RunCompleted {
            run_id: run_id.clone(),
            steps,
        });

        Ok(GraphExecution {
            state,
            run_id,
            graph_id: self.graph_id.clone(),
            root_run_id,
            parent_run_id,
            child_runs: all_child_runs,
            visited,
            steps,
            interrupts: Vec::new(),
            status,
            checkpoint_id: last_checkpoint,
        })
    }

    /// Persists an interrupt boundary and returns the paused execution.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn finish_interrupt(
        &self,
        run_id: &RunId,
        thread_id: &Option<ThreadId>,
        started_at: SystemTime,
        steps: usize,
        async_writes: &mut AsyncCheckpointWrites,
        active: &[Activation],
        goto_map: &HashMap<usize, Vec<RouteTarget>>,
        completed_indices: &[usize],
        interrupts: &[(usize, Interrupt)],
        state: State,
        barrier_arrivals: &mut HashMap<NodeId, HashSet<NodeId>>,
        parent_checkpoint: Option<String>,
        recursion_meta: &serde_json::Value,
        child_runs_meta: &serde_json::Value,
        root_run_id: &RunId,
        parent_run_id: &Option<RunId>,
        all_child_runs: Vec<ChildRun>,
        visited: Vec<NodeId>,
    ) -> Result<GraphExecution<State>> {
        if let Err(err) = self.require_interrupt_durability(thread_id) {
            return self
                .fail_and_return(run_id, thread_id, started_at, steps, async_writes, err)
                .await;
        }
        // Split the step three ways: branches that ran to a result,
        // branches that interrupted, and — under sequential execution —
        // branches that were never started at all.
        //
        // A branch that ran keeps its result and has its successors
        // routed now, so a resume never runs it again. Only the
        // interrupted and never-started ones are rescheduled.
        //
        // Position is not the discriminator. Under parallel execution the
        // whole active set runs before anything is folded, so "after the
        // interrupt" and "did not run" are different sets; rescheduling
        // by position would re-run completed work and fire its side
        // effects a second time. Under sequential execution they happen
        // to coincide, which is exactly why `completed_indices` is
        // reported by the runner rather than inferred here.
        let (completed, completed_goto) =
            Self::partition_completed(active, goto_map, completed_indices);
        let successors =
            match self.route_completed(&completed, &completed_goto, &state, barrier_arrivals) {
                Ok(successors) => successors,
                Err(route_err) => {
                    return self
                        .fail_and_return(
                            run_id,
                            thread_id,
                            started_at,
                            steps,
                            async_writes,
                            route_err,
                        )
                        .await;
                }
            };
        // Rescheduled: the interrupted branches, plus any branch that was
        // never started (sequential execution stops at the interrupt).
        // The branches that ran are represented by their successors
        // instead, so they are not run twice.
        let interrupted_nodes: Vec<NodeId> = interrupts
            .iter()
            .map(|(index, _)| active[*index].node.clone())
            .collect();
        let ran: HashSet<usize> = completed_indices
            .iter()
            .copied()
            .chain(interrupts.iter().map(|(index, _)| *index))
            .collect();
        let mut pending = successors;
        pending.extend(
            interrupts
                .iter()
                .map(|(index, _)| active[*index].clone())
                .chain(
                    active
                        .iter()
                        .enumerate()
                        .filter(|(index, _)| !ran.contains(index))
                        .map(|(_, activation)| activation.clone()),
                ),
        );
        let pending_nodes = activation_nodes(&pending);
        let interrupt_ids: Vec<InterruptId> = interrupts
            .iter()
            .map(|(_, emitted)| InterruptId::new(emitted.id.clone()))
            .collect();
        let emitted_interrupts: Vec<Interrupt> = interrupts
            .iter()
            .map(|(_, emitted)| emitted.clone())
            .collect();
        // An interrupt hands control back to the caller expecting a
        // fully durable pause point: settle any in-flight Async
        // background writes first, failing the run if one was lost
        // (a broken lineage cannot be safely resumed from).
        if let Err(err) = async_writes.drain().await {
            return self
                .fail_and_return(run_id, thread_id, started_at, steps, async_writes, err)
                .await;
        }
        let checkpoint_id = match self
            .persist_checkpoint(
                thread_id,
                run_id,
                &state,
                &pending,
                &completed,
                emitted_interrupts.clone(),
                &interrupted_nodes,
                barrier_arrivals,
                parent_checkpoint.clone(),
                steps,
                "loop",
                recursion_meta,
                child_runs_meta,
            )
            .await
        {
            Ok(id) => id,
            Err(persist_err) => {
                return self
                    .fail_and_return(
                        run_id,
                        thread_id,
                        started_at,
                        steps,
                        async_writes,
                        persist_err,
                    )
                    .await;
            }
        };

        let mut status = self.base_status(run_id, thread_id, started_at);
        status.status = ExecutionStatus::Interrupted;
        status.current_step = steps;
        status.active_nodes = pending_nodes;
        status.pending_interrupts = interrupt_ids;
        status.checkpoint_id = checkpoint_id.clone();
        self.save_status(status.clone()).await;

        Ok(GraphExecution {
            state,
            run_id: run_id.clone(),
            graph_id: self.graph_id.clone(),
            root_run_id: root_run_id.clone(),
            parent_run_id: parent_run_id.clone(),
            child_runs: all_child_runs,
            visited,
            steps,
            interrupts: emitted_interrupts,
            status,
            checkpoint_id,
        })
    }

    /// Emits a [`GraphEvent::RunFailed`] and records a terminal `Failed` status
    /// for a run that aborted with `err`.
    ///
    /// `checkpoint_id` is the resumable failure-boundary checkpoint when the run
    /// left one (a node-handler failure on a checkpointed thread), or `None` for
    /// a structural/non-resumable abort. When present it is recorded on the
    /// status so an observer can locate the checkpoint to `resume`/`retry` from.
    pub(super) async fn fail_run(
        &self,
        run_id: &RunId,
        thread_id: &Option<ThreadId>,
        started_at: SystemTime,
        steps: usize,
        err: &GraphError,
        checkpoint_id: Option<CheckpointId>,
    ) {
        self.emit(GraphEvent::RunFailed {
            run_id: run_id.clone(),
            error: err.to_string(),
        });
        let mut status = self.base_status(run_id, thread_id, started_at);
        status.status = ExecutionStatus::Failed;
        status.current_step = steps;
        status.ended_at = Some(SystemTime::now());
        status.error = Some(err.to_string());
        status.checkpoint_id = checkpoint_id;
        self.save_status(status).await;
    }

    /// Records a terminal `Failed` status for `err` (via [`Self::fail_run`]) and
    /// returns it as `Err`.
    ///
    /// Used at the step boundary so an error raised *after* the node runners —
    /// a reducer merge, a routing resolution, or a checkpoint persist — still
    /// transitions the run to `Failed` (rather than leaving observers to see it
    /// stuck in `Running` forever) before the error unwinds out of the run.
    ///
    /// Any in-flight `Async` background write is drained first: dropping the
    /// tracker would detach those tasks, discarding their outcome (contrary to
    /// [`AsyncCheckpointWrites`]' contract) and racing a caller that
    /// immediately `retry`s the thread. A background write error must not
    /// replace the error that aborted the run, so it is dropped here — exactly
    /// as at the failure boundary.
    pub(super) async fn fail_and_return<T>(
        &self,
        run_id: &RunId,
        thread_id: &Option<ThreadId>,
        started_at: SystemTime,
        steps: usize,
        writes: &mut AsyncCheckpointWrites,
        err: GraphError,
    ) -> Result<T> {
        let _ = writes.drain().await;
        self.fail_run(run_id, thread_id, started_at, steps, &err, None)
            .await;
        Err(err)
    }

    /// Persists a resumable failure-boundary checkpoint for a node-handler
    /// failure that survived the node-retry policy.
    ///
    /// Mirrors the interrupt boundary: `next_nodes` schedules the failed node
    /// (and any not-yet-run members of the step) so `resume`/`retry` re-runs
    /// exactly what did not complete, while `completed_tasks` records the
    /// branches that already succeeded (their updates are folded into `state`
    /// before this is called). The rendered error and failed node id are stamped
    /// into the checkpoint metadata for diagnosis. A no-op returning `None` when
    /// no checkpointer/thread is configured — the run then aborts without a
    /// resumable checkpoint, exactly as before this policy existed.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn persist_failure_checkpoint(
        &self,
        thread_id: &Option<ThreadId>,
        run_id: &RunId,
        state: &State,
        pending: &[Activation],
        completed_tasks: &[Activation],
        barrier_arrivals: &HashMap<NodeId, HashSet<NodeId>>,
        parent: Option<String>,
        step: usize,
        failed_node: &NodeId,
        error: &GraphError,
        recursion: &serde_json::Value,
        child_runs: &serde_json::Value,
    ) -> Result<Option<CheckpointId>> {
        let (Some(checkpointer), Some(thread)) = (&self.checkpointer, thread_id) else {
            return Ok(None);
        };
        let checkpoint = Checkpoint {
            thread_id: thread.to_string(),
            checkpoint_id: next_checkpoint_id(),
            run_id: Some(run_id.to_string()),
            parent_checkpoint_id: parent,
            namespace: self.namespace.clone(),
            state: state.clone(),
            next_nodes: activation_nodes(pending),
            completed_tasks: activation_nodes(completed_tasks),
            pending_writes: Self::completion_writes(completed_tasks, step),
            interrupts: Vec::new(),
            pending_activations: Some(pending.iter().map(PendingActivation::from).collect()),
            barrier_arrivals: barriers_to_persisted(barrier_arrivals),
            metadata: serde_json::json!({
                "source": "loop",
                "step": step,
                "recursion": recursion,
                "child_runs": child_runs,
                "failed_node": failed_node.as_str(),
                "error": error.to_string(),
            }),
        };
        let writes = checkpoint.pending_writes.clone();
        let config = CheckpointConfig {
            thread_id: checkpoint.thread_id.clone(),
            checkpoint_id: Some(checkpoint.checkpoint_id.clone()),
            namespace: checkpoint.namespace.clone(),
        };
        let id = checkpointer.put(checkpoint).await?;
        // Also record the ledger through the write protocol, so backends that
        // implement it can answer "did this task run?" without loading the
        // whole state payload.
        checkpointer.put_writes(&config, &writes).await?;
        self.emit(GraphEvent::CheckpointSaved {
            checkpoint_id: id.clone(),
        });
        Ok(Some(id))
    }
}
