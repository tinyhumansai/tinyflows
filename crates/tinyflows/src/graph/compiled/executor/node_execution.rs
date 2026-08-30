use super::super::*;
use crate::graph::error::{GraphError, Result};

impl<State, Update> CompiledGraph<State, Update>
where
    State: Clone + Send + Sync + 'static,
    Update: Send + 'static,
{
    /// Builds the per-task [`NodeContext`] for `node_id` at the given branch.
    ///
    /// `fork` carries the branch identity in a concurrent step (`None` in
    /// sequential mode or single-node steps). The resume value for the node is
    /// consumed from `resume_map`.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn node_context(
        &self,
        node_id: &NodeId,
        run_id: &RunId,
        thread_id: &Option<ThreadId>,
        step: usize,
        resume_map: &mut HashMap<NodeId, serde_json::Value>,
        fork: Option<ForkId>,
        send_arg: Option<serde_json::Value>,
        root_run_id: &RunId,
        frames: &[RecursionFrame],
        child_runs: &ChildRunSink,
    ) -> NodeContext {
        NodeContext {
            node_id: node_id.clone(),
            run_id: run_id.clone(),
            thread_id: thread_id.clone(),
            step,
            resume: resume_map.remove(node_id),
            fork,
            send_arg,
            root_run_id: Some(root_run_id.clone()),
            recursion_frames: frames.to_vec(),
            child_runs: Some(child_runs.clone()),
        }
    }

    /// Wraps a node future in the configured per-node timeout (if any), mapping
    /// an elapsed deadline onto [`GraphError::Timeout`].
    pub(super) async fn run_node_future(
        &self,
        node_id: &NodeId,
        fut: NodeFuture<Update>,
    ) -> Result<NodeResult<Update>> {
        match self.node_timeout {
            Some(timeout) => match tokio::time::timeout(timeout, fut).await {
                Ok(result) => result,
                Err(_) => Err(GraphError::Timeout(format!(
                    "node `{node_id}` exceeded its {timeout:?} timeout"
                ))),
            },
            None => fut.await,
        }
    }

    /// Runs one node handler.
    ///
    /// Builds the handler future and re-clones the context, then bounds it with
    /// the graph's per-node timeout via [`Self::run_node_future`]. Node-level
    /// retry is deliberately not handled here: tinyflows applies its own
    /// per-node `on_error`/retry policy inside the node handler it installs, so
    /// a second retry loop at this layer would multiply the attempt budget.
    pub(super) async fn run_node_with_retry(
        &self,
        node_id: &NodeId,
        handler: &Arc<NodeHandler<State, Update>>,
        state: &State,
        ctx: NodeContext,
        _step: usize,
    ) -> Result<NodeResult<Update>> {
        let fut = handler(state.clone(), ctx.clone());
        self.run_node_future(node_id, fut).await
    }

    /// Folds a single successful branch result into the step accumulators.
    ///
    /// Pushes the node to `visited`, records updates/goto, emits the matching
    /// events, and returns the interrupt (with its branch index) when the branch
    /// paused. Shared by the sequential and parallel run paths so both fold
    /// results identically; only the *running* of handlers differs.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn fold_result(
        &self,
        index: usize,
        node_id: &NodeId,
        step: usize,
        result: NodeResult<Update>,
        updates: &mut Vec<Update>,
        goto_map: &mut HashMap<usize, Vec<RouteTarget>>,
        visited: &mut Vec<NodeId>,
    ) -> Option<(usize, Interrupt)> {
        visited.push(node_id.clone());
        match result {
            NodeResult::Update(update) => {
                updates.push(update);
                self.emit(GraphEvent::StateUpdated {
                    node: node_id.clone(),
                    step,
                });
            }
            NodeResult::Command(command) => {
                if let Some(update) = command.update {
                    updates.push(update);
                    self.emit(GraphEvent::StateUpdated {
                        node: node_id.clone(),
                        step,
                    });
                }
                if !command.goto.is_empty() {
                    goto_map.insert(index, command.goto);
                }
            }
            NodeResult::Interrupt(emitted) => {
                self.emit(GraphEvent::InterruptEmitted {
                    interrupt: emitted.clone(),
                });
                return Some((index, emitted));
            }
        }
        self.emit(GraphEvent::NodeCompleted {
            node: node_id.clone(),
            step,
        });
        None
    }

    /// Runs the active node set one node at a time (default behavior).
    ///
    /// Short-circuits on the first error (run aborts) or interrupt (later nodes
    /// in the step are not started), exactly preserving milestone-1 semantics.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn run_active_sequential(
        &self,
        active: &[Activation],
        state: &State,
        run_id: &RunId,
        thread_id: &Option<ThreadId>,
        step: usize,
        resume_map: &mut HashMap<NodeId, serde_json::Value>,
        visited: &mut Vec<NodeId>,
        root_run_id: &RunId,
        frames: &[RecursionFrame],
        child_runs: &ChildRunSink,
    ) -> Result<StepRun<Update>> {
        let mut updates: Vec<Update> = Vec::new();
        let mut goto_map: HashMap<usize, Vec<RouteTarget>> = HashMap::new();
        let mut interrupts: Vec<(usize, Interrupt)> = Vec::new();
        let mut completed: Vec<usize> = Vec::new();
        let mut failure: Option<StepFailure> = None;

        for (index, activation) in active.iter().enumerate() {
            let node_id = &activation.node;
            let node = self
                .nodes
                .get(node_id)
                .ok_or_else(|| GraphError::MissingNode(node_id.to_string()))?;

            self.emit(GraphEvent::TaskScheduled {
                node: node_id.clone(),
                step,
            });
            self.emit(GraphEvent::NodeStarted {
                node: node_id.clone(),
                step,
            });

            let ctx = self.node_context(
                node_id,
                run_id,
                thread_id,
                step,
                resume_map,
                None,
                activation.send_arg.clone(),
                root_run_id,
                frames,
                child_runs,
            );
            let result = match self
                .run_node_with_retry(node_id, &node.handler, state, ctx, step)
                .await
            {
                Ok(result) => result,
                Err(error) => {
                    self.emit(GraphEvent::NodeFailed {
                        node: node_id.clone(),
                        step,
                        error: error.to_string(),
                    });
                    // Preserve the progress of the branches that already ran:
                    // the executor records them as completed and schedules their
                    // successors plus this node and the not-yet-run tail for a
                    // resumable retry.
                    failure = Some(StepFailure {
                        failed_index: index,
                        error,
                    });
                    break;
                }
            };

            if let Some(found) = self.fold_result(
                index,
                node_id,
                step,
                result,
                &mut updates,
                &mut goto_map,
                visited,
            ) {
                interrupts.push(found);
                // Sequential execution stops here: the rest of the step is never
                // started, so those branches stay absent from `completed`.
                break;
            }
            completed.push(index);
        }

        Ok(StepRun {
            updates,
            goto_map,
            completed,
            interrupts,
            failure,
        })
    }

    /// Runs the active node set concurrently (opt-in via `with_parallel`).
    ///
    /// Each branch executes on its own cloned `State` snapshot and a distinct
    /// [`ForkId`], optionally with the [`Send`] argument that scheduled it. With
    /// no `max_concurrency` bound every branch starts before any is awaited and
    /// all are driven via [`futures_util::future::join_all`]; with a bound the active
    /// set is run in chunks of at most that many futures, so at most that many
    /// node handlers are in flight at once. Results are folded in active-set
    /// index order — the reducer is the join/fan-in — so the merged state is
    /// reproducible regardless of completion order. The lowest-index branch that
    /// errors or interrupts is the step's terminal outcome; lower-index
    /// successful branches still contribute their updates.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn run_active_parallel(
        &self,
        active: &[Activation],
        state: &State,
        run_id: &RunId,
        thread_id: &Option<ThreadId>,
        step: usize,
        resume_map: &mut HashMap<NodeId, serde_json::Value>,
        visited: &mut Vec<NodeId>,
        root_run_id: &RunId,
        frames: &[RecursionFrame],
        child_runs: &ChildRunSink,
    ) -> Result<StepRun<Update>> {
        // Build one forked context + future per branch. Node lookup and resume
        // consumption happen up front so the futures borrow nothing mutable; each
        // branch drives its handler through the node-retry policy (which also
        // applies the per-node timeout), so a transient failure in one branch is
        // retried without disturbing its siblings.
        let mut futures = Vec::with_capacity(active.len());
        for (index, activation) in active.iter().enumerate() {
            let node_id = &activation.node;
            let node = self
                .nodes
                .get(node_id)
                .ok_or_else(|| GraphError::MissingNode(node_id.to_string()))?;

            self.emit(GraphEvent::TaskScheduled {
                node: node_id.clone(),
                step,
            });
            self.emit(GraphEvent::NodeStarted {
                node: node_id.clone(),
                step,
            });

            self.emit(GraphEvent::ContextForked {
                node: node_id.clone(),
                fork: index,
                step,
            });
            let fork = Some(ForkId::new(index, node_id.clone()));
            let ctx = self.node_context(
                node_id,
                run_id,
                thread_id,
                step,
                resume_map,
                fork,
                activation.send_arg.clone(),
                root_run_id,
                frames,
                child_runs,
            );
            let handler = node.handler.clone();
            let owned_node = node_id.clone();
            // Box each branch future behind a concrete `Send` bound. This keeps
            // the `buffer_unordered` rolling window below (used for a
            // `max_concurrency` bound) from requiring a higher-ranked `Send`
            // proof over the borrowed recursion frames, which the compiler
            // cannot discharge for the bare `async` blocks.
            let fut: std::pin::Pin<
                Box<dyn std::future::Future<Output = Result<NodeResult<Update>>> + Send + '_>,
            > = Box::pin(async move {
                self.run_node_with_retry(&owned_node, &handler, state, ctx, step)
                    .await
            });
            futures.push(fut);
        }

        // Drive branches to completion, bounding in-flight count when configured.
        // With a bound, keep a rolling window of `limit` branches in flight
        // instead of fixed `join_all` chunks. A chunked join runs each chunk to
        // completion before starting the next, so a single slow branch
        // head-of-line blocks the whole chunk; the rolling window starts a new
        // branch as soon as *any* in-flight one finishes. `select_all` reports
        // which pending future completed; a parallel index Vec maps it back to
        // the branch's active-set position, so results are re-ordered into
        // deterministic order for the fold below.
        // A per-node cap binds only when this step activates one node more times
        // than its cap allows — which needs `Send` fanout, since plain activations
        // are deduplicated by node.
        let node_caps_bind = !self.node_concurrency.is_empty() && {
            let mut counts: HashMap<&NodeId, usize> = HashMap::new();
            active.iter().any(|activation| {
                let seen = counts.entry(&activation.node).or_default();
                *seen += 1;
                self.node_concurrency
                    .get(&activation.node)
                    .is_some_and(|cap| *seen > *cap)
            })
        };
        let global_binds = self
            .max_concurrency
            .is_some_and(|limit| limit < futures.len());

        let results = match (global_binds || node_caps_bind).then_some(()) {
            Some(()) => {
                // Admission is governed by two independent ceilings: the
                // graph-wide in-flight count, and how many activations of one
                // *node* may be in flight. A branch starts only when both allow
                // it, so throttling a wide fanout of one node does not also
                // throttle the unrelated branches sharing its step.
                let limit = self.max_concurrency.unwrap_or(futures.len()).max(1);
                let total = futures.len();
                let mut slots: Vec<Option<Result<NodeResult<Update>>>> =
                    (0..total).map(|_| None).collect();
                // Queued branches, in active-set order, each tagged with its node
                // so admission can consult that node's cap.
                let mut queue: std::collections::VecDeque<(usize, _)> =
                    futures.into_iter().enumerate().collect();
                let mut in_flight_per_node: HashMap<NodeId, usize> = HashMap::new();
                let mut running = Vec::with_capacity(limit);
                let mut running_index = Vec::with_capacity(limit);

                // Admits as many queued branches as both ceilings currently
                // allow, preserving active-set order among those admitted.
                macro_rules! admit {
                    () => {
                        while running.len() < limit {
                            let Some(position) = queue.iter().position(|(index, _)| {
                                let node = &active[*index].node;
                                self.node_concurrency.get(node).is_none_or(|cap| {
                                    in_flight_per_node.get(node).copied().unwrap_or(0) < *cap
                                })
                            }) else {
                                // Every queued branch is blocked by its node's
                                // cap; the next completion frees one.
                                break;
                            };
                            let (index, fut) =
                                queue.remove(position).expect("position is in range");
                            *in_flight_per_node
                                .entry(active[index].node.clone())
                                .or_default() += 1;
                            running.push(fut);
                            running_index.push(index);
                        }
                    };
                }

                admit!();
                while !running.is_empty() {
                    let (result, completed, rest) = futures_util::future::select_all(running).await;
                    let index = running_index.remove(completed);
                    if let Some(count) = in_flight_per_node.get_mut(&active[index].node) {
                        *count = count.saturating_sub(1);
                    }
                    slots[index] = Some(result);
                    running = rest;
                    admit!();
                }
                slots
                    .into_iter()
                    .map(|slot| slot.expect("every branch produced a result"))
                    .collect::<Vec<_>>()
            }
            _ => futures_util::future::join_all(futures).await,
        };

        // Fold in deterministic active-set index order.
        let mut updates: Vec<Update> = Vec::new();
        let mut goto_map: HashMap<usize, Vec<RouteTarget>> = HashMap::new();
        let mut interrupts: Vec<(usize, Interrupt)> = Vec::new();
        let mut completed: Vec<usize> = Vec::new();
        let mut failure: Option<StepFailure> = None;

        for (index, (activation, result)) in active.iter().zip(results).enumerate() {
            let node_id = &activation.node;
            let result = match result {
                Ok(result) => result,
                Err(error) => {
                    self.emit(GraphEvent::NodeFailed {
                        node: node_id.clone(),
                        step,
                        error: error.to_string(),
                    });
                    // The lowest-index failing branch is terminal: fold the
                    // lower-index successes (already applied above) and schedule
                    // their successors plus this branch and the rest for a
                    // resumable retry.
                    failure = Some(StepFailure {
                        failed_index: index,
                        error,
                    });
                    break;
                }
            };

            // Deliberately no `break` here, unlike the sequential path.
            //
            // Every branch in this step has already *run* — they were driven
            // concurrently above — so stopping the fold at the first interrupt
            // would discard work that genuinely completed and re-schedule it, and
            // resuming would run those branches a second time. For a node with
            // side effects that means firing them twice. Fold every non-
            // interrupting branch and let the caller schedule only the
            // interrupted ones for resume.
            if let Some(found) = self.fold_result(
                index,
                node_id,
                step,
                result,
                &mut updates,
                &mut goto_map,
                visited,
            ) {
                interrupts.push(found);
            } else {
                completed.push(index);
            }
        }

        Ok(StepRun {
            updates,
            goto_map,
            completed,
            interrupts,
            failure,
        })
    }
}
