use super::super::*;
use crate::graph::error::Result;

impl<State, Update> CompiledGraph<State, Update>
where
    State: Clone + Send + Sync + 'static,
    Update: Send + 'static,
{
    /// Splits a step's active set into the branches that completed, paired with
    /// their routing re-keyed to the compacted set.
    ///
    /// [`Self::route_completed`] keys `goto_map` by position within the slice it
    /// is handed, so dropping the interrupted branches from the middle would
    /// silently misattribute every later branch's routing to the wrong node —
    /// a `Send` packet would end up delivered on someone else's behalf. The
    /// re-keying is the whole reason this is a function rather than a `filter`.
    pub(super) fn partition_completed(
        active: &[Activation],
        goto_map: &HashMap<usize, Vec<RouteTarget>>,
        completed_indices: &[usize],
    ) -> (Vec<Activation>, HashMap<usize, Vec<RouteTarget>>) {
        let mut completed = Vec::with_capacity(completed_indices.len());
        let mut routing = HashMap::new();
        for index in completed_indices {
            let Some(activation) = active.get(*index) else {
                continue;
            };
            if let Some(targets) = goto_map.get(index) {
                routing.insert(completed.len(), targets.clone());
            }
            completed.push(activation.clone());
        }
        (completed, routing)
    }

    /// Routes a set of completed activations into their successor activations.
    ///
    /// Honors per-activation command `goto` (keyed by active-set index), static
    /// and conditional edges, barrier gating (a waiting node is held until every
    /// required predecessor has arrived, accumulating into `barrier_arrivals`
    /// across supersteps), and per-node dedup — while preserving each `Send`
    /// packet's per-invocation argument. Emits a
    /// [`GraphEvent::RouteSelected`] per selected edge.
    ///
    /// Shared by the normal step boundary (routes the whole active set) and the
    /// interrupt/failure boundaries (route just the branches that completed
    /// before the pause, so their successors are still scheduled on resume).
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn persist_checkpoint(
        &self,
        thread_id: &Option<ThreadId>,
        run_id: &RunId,
        state: &State,
        pending: &[Activation],
        completed_tasks: &[Activation],
        interrupts: Vec<Interrupt>,
        interrupted: &[NodeId],
        barrier_arrivals: &HashMap<NodeId, HashSet<NodeId>>,
        parent: Option<String>,
        step: usize,
        source: &str,
        recursion: &serde_json::Value,
        child_runs: &serde_json::Value,
    ) -> Result<Option<CheckpointId>> {
        let (Some(checkpointer), Some(thread)) = (&self.checkpointer, thread_id) else {
            return Ok(None);
        };
        let checkpoint = self.build_loop_checkpoint(
            thread,
            run_id,
            state,
            pending,
            completed_tasks,
            interrupts,
            interrupted,
            barrier_arrivals,
            parent,
            step,
            source,
            recursion,
            child_runs,
        );
        let writes = checkpoint.pending_writes.clone();
        let config = CheckpointConfig {
            thread_id: checkpoint.thread_id.clone(),
            checkpoint_id: Some(checkpoint.checkpoint_id.clone()),
            namespace: checkpoint.namespace.clone(),
        };
        let id = checkpointer.put(checkpoint).await?;
        checkpointer.put_writes(&config, &writes).await?;
        self.emit(GraphEvent::CheckpointSaved {
            checkpoint_id: id.clone(),
        });
        Ok(Some(id))
    }

    /// Persists a boundary checkpoint without blocking the superstep loop
    /// ([`DurabilityMode::Async`]).
    ///
    /// The checkpoint id is minted up front and returned immediately so the
    /// loop keeps chaining lineage onto it, while the actual `put` (and the
    /// [`GraphEvent::CheckpointSaved`] emitted on its success) runs on a
    /// spawned background task tracked in `writes`.
    ///
    /// # Failure semantics
    ///
    /// A background write error is never dropped: it is recorded in `writes`
    /// and surfaced by the executor at the next durability boundary, or at the
    /// latest when the run drains all in-flight writes at its terminal /
    /// interrupt boundary — so the run result reflects persistence failures.
    /// Because the `CheckpointSaved` event is emitted from the background
    /// task, its ordering relative to subsequent step events is not
    /// deterministic under `Async` durability.
    ///
    /// Outside a tokio runtime there is nothing to spawn onto, so the write
    /// happens inline — degrading to [`DurabilityMode::Sync`] behavior.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn persist_checkpoint_nonblocking(
        &self,
        writes: &mut AsyncCheckpointWrites,
        thread_id: &Option<ThreadId>,
        run_id: &RunId,
        state: &State,
        pending: &[Activation],
        completed_tasks: &[Activation],
        barrier_arrivals: &HashMap<NodeId, HashSet<NodeId>>,
        parent: Option<String>,
        step: usize,
        recursion: &serde_json::Value,
        child_runs: &serde_json::Value,
    ) -> Result<Option<CheckpointId>> {
        let (Some(checkpointer), Some(thread)) = (&self.checkpointer, thread_id) else {
            return Ok(None);
        };
        let checkpoint = self.build_loop_checkpoint(
            thread,
            run_id,
            state,
            pending,
            completed_tasks,
            Vec::new(),
            &[],
            barrier_arrivals,
            parent,
            step,
            "loop",
            recursion,
            child_runs,
        );
        let id = CheckpointId::new(checkpoint.checkpoint_id.clone());

        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                let checkpointer = Arc::clone(checkpointer);
                let sink = self.event_sink.clone();
                writes.spawn_ordered(&handle, async move {
                    let id = checkpointer.put(checkpoint).await?;
                    if let Some(sink) = sink {
                        sink.emit(GraphEvent::CheckpointSaved {
                            checkpoint_id: id.clone(),
                        });
                    }
                    Ok(id)
                });
                Ok(Some(id))
            }
            Err(_) => {
                let id = checkpointer.put(checkpoint).await?;
                self.emit(GraphEvent::CheckpointSaved {
                    checkpoint_id: id.clone(),
                });
                Ok(Some(id))
            }
        }
    }

    /// Records completion markers for the tasks that finished in the step a
    /// boundary checkpoint closes.
    ///
    /// A graph's `Update` carries no `Serialize` bound, so the executor cannot
    /// persist *what* a task wrote — but it does not need to: the applied value
    /// is already durable in the checkpoint's `state`. What was missing was the
    /// other half, the per-task record of *that* it ran, which is what lets a
    /// resume distinguish "already done" from "not yet started". See
    /// [`PendingWrite`](crate::graph::checkpoint::PendingWrite)'s docs for why
    /// that distinction is the whole point of
    /// the ledger.
    ///
    /// The task id is persisted on the activation itself, so a resume can
    /// match a marker to one fan-out task rather than every task with its node.
    pub(super) fn completion_writes(
        completed_tasks: &[Activation],
        _step: usize,
    ) -> Vec<crate::graph::checkpoint::PendingWrite> {
        completed_tasks
            .iter()
            .map(|activation| {
                crate::graph::checkpoint::PendingWrite::completion_marker(
                    activation.node.clone(),
                    activation.task_id.clone(),
                )
            })
            .collect()
    }

    /// Builds the loop-boundary [`Checkpoint`] record shared by the sync and
    /// async persist paths, minting a fresh checkpoint id.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn build_loop_checkpoint(
        &self,
        thread: &ThreadId,
        run_id: &RunId,
        state: &State,
        pending: &[Activation],
        completed_tasks: &[Activation],
        interrupts: Vec<Interrupt>,
        interrupted: &[NodeId],
        barrier_arrivals: &HashMap<NodeId, HashSet<NodeId>>,
        parent: Option<String>,
        step: usize,
        source: &str,
        recursion: &serde_json::Value,
        child_runs: &serde_json::Value,
    ) -> Checkpoint<State> {
        let mut metadata = serde_json::json!({
            "source": source,
            "step": step,
            "recursion": recursion,
            "child_runs": child_runs,
        });
        // Which node of *this* graph paused, as opposed to the (possibly
        // re-emitted, child-owned) `Interrupt::node`. Resume keys the resume
        // value on it; omitted entirely when nothing interrupted.
        if !interrupted.is_empty() {
            metadata["interrupted_nodes"] = serde_json::json!(
                interrupted
                    .iter()
                    .map(|n| n.to_string())
                    .collect::<Vec<_>>()
            );
        }
        Checkpoint {
            thread_id: thread.to_string(),
            checkpoint_id: next_checkpoint_id(),
            run_id: Some(run_id.to_string()),
            parent_checkpoint_id: parent,
            namespace: self.namespace.clone(),
            state: state.clone(),
            next_nodes: activation_nodes(pending),
            completed_tasks: activation_nodes(completed_tasks),
            pending_writes: Self::completion_writes(completed_tasks, step),
            pending_activations: Some(pending.iter().map(PendingActivation::from).collect()),
            barrier_arrivals: barriers_to_persisted(barrier_arrivals),
            interrupts,
            metadata,
        }
    }

    pub(super) fn base_status(
        &self,
        run_id: &RunId,
        thread_id: &Option<ThreadId>,
        started_at: SystemTime,
    ) -> GraphRunStatus {
        let mut status = GraphRunStatus::new(
            run_id.clone(),
            self.graph_id.clone(),
            ExecutionStatus::Running,
        );
        status.thread_id = thread_id.clone();
        status.checkpoint_namespace = self.namespace.clone();
        status.started_at = started_at;
        status.updated_at = SystemTime::now();
        status
    }
}
