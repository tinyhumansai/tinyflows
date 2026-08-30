//! Routing: resolving a completed step's active set into the next
//! superstep's activations (`goto`/conditional branches, [`Send`]
//! fanout, and interrupt-durability preconditions).
//!
//! Split out of `compiled/mod.rs`; see that module's doc comment for the
//! executor's overall design.

use super::*;
use crate::graph::error::{GraphError, Result};

impl<State, Update> CompiledGraph<State, Update>
where
    State: Clone + Send + Sync + 'static,
    Update: Send + 'static,
{
    pub(super) fn route_completed(
        &self,
        completed: &[Activation],
        goto_map: &HashMap<usize, Vec<RouteTarget>>,
        state: &State,
        barrier_arrivals: &mut HashMap<NodeId, HashSet<NodeId>>,
    ) -> Result<Vec<Activation>> {
        let mut next: Vec<Activation> = Vec::new();
        let mut next_seen: HashSet<NodeId> = HashSet::new();
        // Resolved targets per activation index, captured once here and
        // reused by the barrier-relief pass below instead of calling
        // `self.route` a second time — a router closure is only guaranteed
        // pure/idempotent per the `route`/`add_conditional_edges` contract,
        // not safe to invoke twice for the same activation.
        let mut resolved: Vec<Vec<RouteTarget>> = Vec::with_capacity(completed.len());
        for (index, activation) in completed.iter().enumerate() {
            let node_id = &activation.node;
            let targets = self.route(node_id, goto_map.get(&index).map(Vec::as_slice), state)?;
            resolved.push(targets.clone());
            for target in targets {
                let tnode = target.node().clone();
                if tnode.as_str() == END {
                    continue;
                }
                self.emit(GraphEvent::RouteSelected {
                    node: node_id.clone(),
                    target: tnode.clone(),
                });
                // Barrier gating: hold a waiting node until every required
                // predecessor has arrived (possibly across supersteps).
                //
                // Only arrivals from a **required** predecessor are gated. An
                // arrival from anywhere else was never part of this barrier's
                // contract, so holding it would be waiting for a rendezvous it
                // is not attending.
                //
                // That distinction is what lets a barrier node also be the head
                // of a cycle. A back-edge is registered as a plain edge, not a
                // waiting one, so its source is absent from `required` — but the
                // gate is keyed on the *target*, so without this check the
                // re-entry would be swallowed on every pass: `arrived` would
                // gain the body's id, still fail the `is_subset` test against
                // the forward predecessors, and `continue`. The loop would run
                // its first pass and then silently stop. Barrier relief cannot
                // rescue that either, since it only fires for a predecessor
                // whose branch was *not* taken, and a fan-in head's forward
                // predecessors did run.
                if let Some(required) = self.waiting.get(&tnode)
                    && required.contains(node_id)
                {
                    let arrived = barrier_arrivals.entry(tnode.clone()).or_default();
                    arrived.insert(node_id.clone());
                    if !required.is_subset(arrived) {
                        continue;
                    }
                    barrier_arrivals.remove(&tnode);
                }
                // `Send` activations may repeat the same node (each carries its
                // own arg); plain activations are deduplicated by node.
                let send_arg = target.send_arg().cloned();
                if send_arg.is_some() {
                    next.push(Activation {
                        node: tnode,
                        send_arg,
                        task_id: String::new(),
                    });
                } else if next_seen.insert(tnode.clone()) {
                    next.push(Activation {
                        node: tnode,
                        send_arg: None,
                        task_id: String::new(),
                    });
                }
            }
        }

        // Mixed fan-in barrier relief: a waiting/barrier node normally
        // activates only once every registered predecessor has arrived. When
        // one of those predecessors is reachable only via a conditional
        // branch, and the *taken* branch does not lead toward it, that
        // predecessor will never arrive on its own — register a phantom
        // arrival on its behalf so the barrier can still clear on the
        // predecessors that actually ran, instead of deadlocking forever.
        //
        // The check is keyed on `source`'s *resolved routing target* this
        // step (via `reaches_deterministically`), not on whether
        // `relief_node` is freshly scheduled into `next` this same
        // superstep. A same-superstep check is wrong for a multi-hop
        // conditional predecessor (`source --branch--> x --main-->
        // relief_node`, `x` a plain pass-through): `relief_node` would not
        // yet be in `next` on the step `source` itself completes — even on
        // the *taken* branch, where `relief_node` WILL run once `x` does —
        // so a same-superstep check would fire a premature phantom arrival
        // and let the barrier clear before the real predecessor's data
        // commits, reintroducing the exact data-loss bug this primitive
        // exists to prevent. Resolving `source`'s actual target and walking
        // it forward through deterministic (non-branching) edges is correct
        // for both direct and multi-hop cases because it answers "was the
        // branch leading to `relief_node` taken", not "did `relief_node`
        // happen to run in lockstep with `source`".
        for relief in self.barrier_reliefs.iter() {
            let source_indices: Vec<usize> = completed
                .iter()
                .enumerate()
                .filter(|(_, activation)| activation.node == relief.source)
                .map(|(index, _)| index)
                .collect();
            if source_indices.is_empty() {
                continue;
            }
            let branch_taken = source_indices.iter().any(|index| {
                resolved[*index].iter().any(|target| {
                    self.reaches_deterministically(
                        target.node(),
                        &relief.relief_node,
                        &relief.barrier_node,
                    )
                })
            });
            // A relief_node freshly scheduled into `next` this step (a
            // direct, single-hop predecessor) is also proof the branch was
            // taken; kept as a defensive fallback alongside the resolved-
            // target check above.
            if branch_taken || next_seen.contains(&relief.relief_node) {
                continue;
            }
            let Some(required) = self.waiting.get(&relief.barrier_node) else {
                continue;
            };
            // Is this barrier participating in the current pass at all?
            //
            // Relief exists to unblock a barrier that is holding real data while
            // one of its predecessors can no longer arrive. It must never be the
            // reason a barrier fires with *nothing* behind it. So before
            // phantoming anything, check that the route actually taken still
            // leads to at least one of the barrier's predecessors — or that one
            // has already arrived.
            //
            // The two cases this separates look identical from a single relief
            // registration, which is why the check is per barrier rather than
            // per predecessor:
            //
            // - A conditional join where one arm was chosen: the taken route
            //   reaches that arm, so the barrier is engaged and the *other* arm
            //   is correctly phantomed. The phantom is needed here before any
            //   real arrival, since the chosen arm has not run yet.
            // - A loop body's join on the pass where the head leaves through
            //   `done`: the taken route reaches neither arm. Nothing will ever
            //   arrive, so the barrier is simply not part of this pass. Firing it
            //   anyway would activate it on empty input and — because its
            //   back-edge re-enters the head, which exits and relieves again —
            //   ping-pong the run forever instead of letting it finish.
            let already_arrived = barrier_arrivals
                .get(&relief.barrier_node)
                .is_some_and(|arrived| !arrived.is_empty());
            let barrier_engaged = already_arrived
                || required.iter().any(|predecessor| {
                    source_indices.iter().any(|index| {
                        resolved[*index].iter().any(|target| {
                            self.reaches_deterministically(
                                target.node(),
                                predecessor,
                                &relief.barrier_node,
                            )
                        })
                    })
                });
            if !barrier_engaged {
                continue;
            }
            let arrived = barrier_arrivals
                .entry(relief.barrier_node.clone())
                .or_default();
            arrived.insert(relief.relief_node.clone());
            if !required.is_subset(arrived) {
                continue;
            }
            barrier_arrivals.remove(&relief.barrier_node);
            if next_seen.insert(relief.barrier_node.clone()) {
                next.push(Activation {
                    node: relief.barrier_node.clone(),
                    send_arg: None,
                    task_id: String::new(),
                });
            }
        }

        Ok(next)
    }

    /// Whether `to` is reachable from `from` by following only deterministic
    /// static routing — plain/waiting edges (`self.edges`), which resolve to
    /// exactly one successor with no runtime decision — without ever
    /// expanding through `stop`.
    ///
    /// This is what makes barrier-relief evaluation correct for a
    /// multi-hop conditional predecessor: once a brancher's routing decision
    /// for this step is resolved to a concrete target, whether that target
    /// eventually leads to a barrier's conditional predecessor is a static
    /// property of the compiled topology for any chain of plain pass-through
    /// nodes — it does not depend on when each hop happens to run. A further
    /// conditional node along the way is a second runtime decision this walk
    /// cannot resolve ahead of time, so it stops there (falling back to the
    /// same-superstep check).
    ///
    /// An **unconditional fan-out** is the one command node the walk does cross.
    /// It has no `self.edges` entry, because its successors come from the
    /// `Command` it emits rather than from a static edge — but every one of its
    /// declared destinations runs whenever it runs, so "does this lead to `to`"
    /// is still a static question. Stopping there instead is not the safe
    /// default it looks like: reporting unreachable is what *fires* relief, so a
    /// fan-out on the path would clear a barrier before its real predecessors
    /// had run and the join would read the previous pass's data. Erring toward
    /// "reachable" costs at worst a barrier that waits, which is loud; erring
    /// the other way is silently wrong output.
    ///
    /// Because a fan-out has several successors this is a search over a DAG
    /// rather than a walk down a chain.
    fn reaches_deterministically(&self, from: &NodeId, to: &NodeId, stop: &NodeId) -> bool {
        if from == to {
            return true;
        }
        let mut seen: HashSet<&NodeId> = HashSet::new();
        let mut frontier: Vec<&NodeId> = vec![from];
        while let Some(current) = frontier.pop() {
            if !seen.insert(current) {
                continue;
            }
            // A plain/waiting edge: exactly one successor, no decision.
            if let Some(next) = self.edges.get(current) {
                if next == to {
                    return true;
                }
                if next != stop {
                    frontier.push(next);
                }
            }
            // An unconditional fan-out: every declared destination runs.
            if self
                .node_meta
                .get(current)
                .is_some_and(|meta| meta.command_fanout)
            {
                for next in &self.node_meta[current].command_destinations {
                    if next == to {
                        return true;
                    }
                    if next != stop {
                        frontier.push(next);
                    }
                }
            }
        }
        false
    }

    /// Resolves the next routing targets for `node_id`.
    ///
    /// Command `goto` (which may include [`Send`] packets) wins over static and
    /// conditional edges; edge/conditional targets are plain node activations.
    ///
    /// `goto` carries this specific activation's [`Command::goto`] targets (when
    /// it returned a command), passed per-activation rather than looked up by
    /// node id so repeated activations of one node never share routing.
    pub(super) fn route(
        &self,
        node_id: &NodeId,
        goto: Option<&[RouteTarget]>,
        state: &State,
    ) -> Result<Vec<RouteTarget>> {
        if let Some(targets) = goto {
            self.validate_route_targets(node_id, targets)?;
            return Ok(targets.to_vec());
        }
        if let Some(target) = self.edges.get(node_id) {
            return Ok(vec![RouteTarget::Node(target.clone())]);
        }
        if let Some(branch) = self.branches.get(node_id) {
            let route = (branch.router)(state);
            let target =
                branch
                    .routes
                    .get(&route)
                    .cloned()
                    .ok_or_else(|| GraphError::MissingRoute {
                        node: node_id.to_string(),
                        route,
                    })?;
            return Ok(vec![RouteTarget::Node(target)]);
        }
        // Sink: no outgoing routing, the branch ends here.
        Ok(Vec::new())
    }

    fn validate_route_targets(&self, node_id: &NodeId, targets: &[RouteTarget]) -> Result<()> {
        for target in targets {
            let target_node = target.node();
            if target_node.as_str() == END {
                continue;
            }
            if target_node.as_str() == START {
                return Err(GraphError::Graph(format!(
                    "command goto from node `{node_id}` cannot target START"
                )));
            }
            if !self.nodes.contains_key(target_node) {
                return Err(GraphError::MissingNode(target_node.to_string()));
            }
        }
        Ok(())
    }

    pub(super) fn require_interrupt_durability(&self, thread_id: &Option<ThreadId>) -> Result<()> {
        if self.checkpointer.is_none() {
            return Err(GraphError::Resume(
                "interrupt emitted without a configured checkpointer".to_string(),
            ));
        }
        if thread_id.is_none() {
            return Err(GraphError::Resume(
                "interrupt emitted without a thread id".to_string(),
            ));
        }
        Ok(())
    }
}
