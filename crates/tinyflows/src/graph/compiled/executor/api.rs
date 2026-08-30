use super::super::*;
use crate::graph::error::{GraphError, Result};

impl<State, Update> CompiledGraph<State, Update>
where
    State: Clone + Send + Sync + 'static,
    Update: Send + 'static,
{
    /// Runs the graph to completion (or to an interrupt) without a thread.
    ///
    /// Without a thread id no checkpoints are persisted even if a checkpointer
    /// is configured, since checkpoints are keyed by thread.
    pub async fn run(&self, state: State) -> Result<GraphExecution<State>> {
        self.execute(
            state,
            vec![Activation::node(self.entry.clone())],
            None,
            HashMap::new(),
            HashMap::new(),
            None,
        )
        .await
    }

    /// Runs the graph with one or more external inputs in the first superstep.
    ///
    /// [`GraphInput::start`] targets the graph's compiled entry node, preserving
    /// the usual `START -> entry` contract for user input. Additional inputs may
    /// target any real node directly, so separate LLM/tool loops can be seeded
    /// together. Inputs are not deduplicated: two inputs aimed at the same node
    /// produce two separate activations, each with its own
    /// [`NodeContext::send_arg`](crate::graph::NodeContext::send_arg).
    pub async fn run_with_inputs(
        &self,
        state: State,
        inputs: impl IntoIterator<Item = GraphInput>,
    ) -> Result<GraphExecution<State>> {
        let active = self.initial_inputs(inputs)?;
        self.execute(state, active, None, HashMap::new(), HashMap::new(), None)
            .await
    }

    /// Runs the graph under a thread id, persisting checkpoints at every
    /// superstep boundary when a checkpointer is configured.
    pub async fn run_with_thread(
        &self,
        thread_id: impl Into<ThreadId>,
        state: State,
    ) -> Result<GraphExecution<State>> {
        self.execute(
            state,
            vec![Activation::node(self.entry.clone())],
            Some(thread_id.into()),
            HashMap::new(),
            HashMap::new(),
            None,
        )
        .await
    }

    /// Runs the graph under a thread id with one or more external inputs in the
    /// first superstep, persisting checkpoints at every boundary when a
    /// checkpointer is configured.
    pub async fn run_with_thread_inputs(
        &self,
        thread_id: impl Into<ThreadId>,
        state: State,
        inputs: impl IntoIterator<Item = GraphInput>,
    ) -> Result<GraphExecution<State>> {
        let active = self.initial_inputs(inputs)?;
        self.execute(
            state,
            active,
            Some(thread_id.into()),
            HashMap::new(),
            HashMap::new(),
            None,
        )
        .await
    }

    /// Resumes an interrupted run from its latest checkpoint, re-running the
    /// interrupted node(s) with the resume value supplied by `command`.
    ///
    /// Requires a checkpointer and an existing checkpoint for the thread;
    /// otherwise returns [`GraphError::Resume`].
    pub async fn resume(
        &self,
        thread_id: impl Into<ThreadId>,
        command: Command<Update>,
    ) -> Result<GraphExecution<State>> {
        self.resume_from(thread_id, ResumeTarget::Latest, command)
            .await
    }

    /// Retries a failed run from its latest (failure-boundary) checkpoint,
    /// re-running the node that failed and the not-yet-run tail of that step.
    ///
    /// This is the resume counterpart for the *failure* path (as opposed to a
    /// human interrupt): after a node handler aborts a checkpointed run — a
    /// transient outage that outlived the node-retry policy, or a hard crash —
    /// the run leaves a resumable checkpoint (see
    /// [`CompiledGraph::with_node_retry`]). Calling `retry` re-runs exactly what
    /// did not complete, carrying no resume value. It is shorthand for
    /// [`CompiledGraph::resume`] with an empty [`Command`].
    ///
    /// To continue on *user feedback* instead of a bare retry, first inspect the
    /// committed state with
    /// [`get_state`](CompiledGraph::get_state), edit it with
    /// [`update_state`](CompiledGraph::update_state), then call `retry` (or
    /// `resume`) — the edited state is what the re-run sees.
    pub async fn retry(&self, thread_id: impl Into<ThreadId>) -> Result<GraphExecution<State>> {
        self.resume_from(thread_id, ResumeTarget::Latest, Command::new())
            .await
    }

    /// Resumes a run from a specific checkpoint (time-travel resume).
    ///
    /// [`ResumeTarget::Latest`] behaves exactly like [`CompiledGraph::resume`];
    /// [`ResumeTarget::Checkpoint`] replays forward from an older checkpoint's
    /// config — re-running its pending nodes (and applying `command`'s resume
    /// value to any interrupted node) without mutating the original record. The
    /// addressed checkpoint is read-only; the replay appends new boundary
    /// checkpoints to the thread rather than rewriting history.
    ///
    /// Requires a checkpointer and a matching checkpoint with pending nodes;
    /// otherwise returns [`GraphError::Resume`].
    pub async fn resume_from(
        &self,
        thread_id: impl Into<ThreadId>,
        target: ResumeTarget,
        command: Command<Update>,
    ) -> Result<GraphExecution<State>> {
        let checkpointer = self
            .checkpointer
            .as_ref()
            .ok_or_else(|| GraphError::Resume("no checkpointer configured".to_string()))?;
        let thread_id = thread_id.into();

        let checkpoint_id = match &target {
            ResumeTarget::Latest => None,
            ResumeTarget::Checkpoint(id) => Some(id.as_str()),
        };
        let checkpoint = checkpointer
            .get_scoped(thread_id.as_str(), checkpoint_id, &self.namespace)
            .await?
            .ok_or_else(|| match &target {
                ResumeTarget::Latest => {
                    GraphError::Resume(format!("no checkpoint found for thread `{thread_id}`"))
                }
                ResumeTarget::Checkpoint(id) => GraphError::Resume(format!(
                    "no checkpoint `{id}` found for thread `{thread_id}`"
                )),
            })?;
        // Resume *loads* this checkpoint — it is a read, not a write — so emit a
        // restore event, not `CheckpointSaved` (which would falsely inflate
        // persisted-checkpoint counts and mislead durability observers).
        self.emit(GraphEvent::CheckpointRestored {
            checkpoint_id: CheckpointId::new(checkpoint.checkpoint_id.clone()),
        });

        // Prefer the persisted pending activations (which preserve each pending
        // node's `Send` arg); fall back to the node-id projection for
        // checkpoints written before that field existed.
        let active: Vec<Activation> = match &checkpoint.pending_activations {
            Some(pending) if !pending.is_empty() => pending.iter().map(Activation::from).collect(),
            _ => checkpoint
                .next_nodes
                .iter()
                .cloned()
                .map(Activation::node)
                .collect(),
        };
        if active.is_empty() {
            return Err(GraphError::Resume(
                "checkpoint has no pending nodes to resume".to_string(),
            ));
        }

        // Partial-failure guard. The boundary that produced this checkpoint
        // recorded a completion marker per task that had already finished; a
        // node named by *both* the pending set and that ledger has therefore
        // already run, and re-running it would repeat its side effects. On a
        // checkpoint the executor itself wrote the two sets are disjoint, so
        // this is a no-op — it earns its keep on a checkpoint that was
        // hand-built, time-travelled to, or edited through `update_state`,
        // where `next_nodes` can legitimately disagree with what ran.
        let completed_config = CheckpointConfig {
            thread_id: thread_id.to_string(),
            checkpoint_id: Some(checkpoint.checkpoint_id.clone()),
            namespace: self.namespace.clone(),
        };
        let recorded = checkpointer.get_writes(&completed_config).await?;
        let done: HashSet<String> = if recorded.is_empty() {
            checkpoint
                .pending_writes
                .iter()
                .map(|w| w.task_id.clone())
                .collect()
        } else {
            recorded.iter().map(|w| w.task_id.clone()).collect()
        };
        let active: Vec<Activation> = if done.is_empty() {
            active
        } else {
            let filtered: Vec<Activation> = active
                .iter()
                // A node name is not a task identity: a Send fan-out can have
                // several live activations of one node. Legacy checkpoints
                // have no persisted task id, so leave them runnable.
                .filter(|a| a.task_id.is_empty() || !done.contains(&a.task_id))
                .cloned()
                .collect();
            if filtered.is_empty() {
                // Every pending node claims to have run. Trust the pending set
                // rather than turning a resumable checkpoint into a hard error:
                // a wrong re-run is recoverable, a stuck thread is not.
                tracing::warn!(
                    "[graph:resume] every pending node of checkpoint `{}` has a completion \
                     marker; resuming them anyway rather than stranding the thread",
                    checkpoint.checkpoint_id
                );
                active
            } else {
                if filtered.len() != active.len() {
                    tracing::debug!(
                        "[graph:resume] checkpoint `{}`: skipping {} already-completed task(s)",
                        checkpoint.checkpoint_id,
                        active.len() - filtered.len()
                    );
                }
                filtered
            }
        };

        // The resume value belongs to the node(s) that actually interrupted. The
        // pending set is deliberately wider than that at an interrupt boundary
        // (it also carries the successors of branches that completed before the
        // interrupt), so fanning the value across it would hand `ctx.resume` to
        // nodes that have never run. A boundary that recorded no interrupt (a
        // failure boundary, resumed via `retry` with no value) keeps the old
        // fan-across-pending behaviour.
        let mut resume_map = HashMap::new();
        if let Some(value) = command.resume {
            let interrupted = interrupted_nodes(&checkpoint, &active);
            if interrupted.is_empty() {
                for activation in &active {
                    resume_map.insert(activation.node.clone(), value.clone());
                }
            } else {
                for node in interrupted {
                    resume_map.insert(node, value.clone());
                }
            }
        }

        // Restore accumulated barrier arrivals so a join's precondition survives
        // the interrupt/failure boundary this checkpoint recorded.
        let initial_barriers = barriers_from_persisted(&checkpoint.barrier_arrivals);
        // Chain the first post-resume boundary onto the checkpoint we loaded so
        // the lineage spine stays connected across the resume.
        let initial_parent = Some(checkpoint.checkpoint_id.clone());

        self.execute(
            checkpoint.state,
            active,
            Some(thread_id),
            resume_map,
            initial_barriers,
            initial_parent,
        )
        .await
    }

    pub(super) fn initial_inputs(
        &self,
        inputs: impl IntoIterator<Item = GraphInput>,
    ) -> Result<Vec<Activation>> {
        let mut active = Vec::new();
        for input in inputs {
            let node = if input.node.as_str() == START {
                self.entry.clone()
            } else if input.node.as_str() == END {
                return Err(GraphError::Graph(
                    "graph input cannot target END".to_string(),
                ));
            } else {
                if !self.nodes.contains_key(&input.node) {
                    return Err(GraphError::MissingNode(input.node.to_string()));
                }
                input.node
            };
            active.push(Activation {
                node,
                send_arg: input.payload,
                task_id: String::new(),
            });
        }
        if active.is_empty() {
            return Err(GraphError::Validation(
                "run_with_inputs requires at least one input".to_string(),
            ));
        }
        Ok(active)
    }

    // ---- State inspection & time travel ------------------------------------

    /// Returns the configured checkpointer or a [`GraphError::Checkpoint`]
    /// when inspection is attempted on a graph without durability.
    pub(super) async fn execute(
        &self,
        state: State,
        initial_active: Vec<Activation>,
        thread_id: Option<ThreadId>,
        resume_map: HashMap<NodeId, serde_json::Value>,
        initial_barriers: HashMap<NodeId, HashSet<NodeId>>,
        initial_parent: Option<String>,
    ) -> Result<GraphExecution<State>> {
        let run_id = crate::graph::ids::new_run_id();
        // When a durable journal is configured, run against a clone whose event
        // sink wraps every emitted event into a `GraphObservation` and appends
        // it (while still forwarding to any pre-existing live sink). The journal
        // sink carries this graph's checkpoint namespace so subgraph runs record
        // their nested path. Default (no journal) leaves `self` untouched.
        if self.journal.is_some() {
            let this = self.clone_with_journal_sink(&run_id, &thread_id);
            this.execute_run(
                run_id,
                state,
                initial_active,
                thread_id,
                resume_map,
                initial_barriers,
                initial_parent,
            )
            .await
        } else {
            self.execute_run(
                run_id,
                state,
                initial_active,
                thread_id,
                resume_map,
                initial_barriers,
                initial_parent,
            )
            .await
        }
    }

    /// Builds a clone whose `event_sink` is a [`JournalGraphSink`] for `run_id`,
    /// wrapping any existing sink as the live downstream. Returns a plain clone
    /// when no journal is configured.
    pub(super) fn clone_with_journal_sink(
        &self,
        run_id: &RunId,
        thread_id: &Option<ThreadId>,
    ) -> Self {
        let Some(journal) = &self.journal else {
            return self.clone();
        };
        let mut sink = crate::graph::observability::JournalGraphSink::new(
            journal.clone(),
            run_id.clone(),
            self.graph_id.clone(),
        )
        .with_namespace(self.namespace.clone())
        .with_thread(thread_id.clone());
        if let Some(inner) = &self.event_sink {
            sink = sink.with_inner(inner.clone());
        }
        let mut this = self.clone();
        this.event_sink = Some(Arc::new(sink));
        this
    }

    /// Best-effort status write; never aborts the run on a status-store error.
    pub(super) async fn save_status(&self, status: GraphRunStatus) {
        if let Some(store) = &self.status_store {
            let _ = store.put_status(status).await;
        }
    }
}
