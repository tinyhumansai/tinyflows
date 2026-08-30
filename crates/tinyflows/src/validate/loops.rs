use super::*;

/// Loop and cycle legality.
///
/// **Cycles are legal.** The engine lowers a back-edge as a plain re-entry and
/// the executor underneath is a super-step scheduler, so a graph that loops is
/// a supported graph, not a malformed one. What this pass refuses is the
/// narrow set of cycles that *cannot work* — each with a message naming the
/// fix, because "your loop silently ran once and stopped" is the failure mode
/// this replaces.
pub(super) fn validate_loops(graph: &WorkflowGraph, errors: &mut Vec<ValidationError>) {
    let loop_edges = crate::engine::back_edges(graph);

    // Per-kind `loop` config, checked whether or not the node is actually wired
    // into a cycle: a misconfigured loop head is worth naming either way.
    for node in graph.nodes.iter().filter(|n| n.kind == NodeKind::Loop) {
        if let Some(max) = node.config.get("max_iterations") {
            match max.as_u64() {
                Some(n) if n > 0 => {}
                _ => errors.push(ValidationError::InvalidNodeConfig {
                    node: node.id.clone(),
                    reason: "loop `max_iterations` must be a positive integer".to_string(),
                }),
            }
        }
        if let Some(state) = node.config.get("state")
            && !state.is_object()
        {
            errors.push(ValidationError::InvalidNodeConfig {
                node: node.id.clone(),
                reason: "loop `state` must be an object with `init` and/or `update`".to_string(),
            });
        }
        if let Some(emit) = node.config.get("emit")
            && !matches!(emit.as_str(), Some("items") | Some("state") | Some("both"))
        {
            errors.push(ValidationError::InvalidNodeConfig {
                node: node.id.clone(),
                reason: format!("loop `emit` must be \"items\", \"state\" or \"both\", got {emit}"),
            });
        }
        // A `success` exit that goes nowhere strands the converged case: the
        // run would simply end there, which looks like the loop never finished.
        if node
            .config
            .get("success_port")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            && !graph
                .edges
                .iter()
                .any(|e| e.from_node == node.id && e.from_port == "success")
        {
            errors.push(ValidationError::InvalidNodeConfig {
                node: node.id.clone(),
                reason: "loop sets `success_port: true` but nothing is wired to its `success` \
                         port, so a converged loop would strand the run; wire `success` or drop \
                         the flag"
                    .to_string(),
            });
        }
        if let Some(policy) = node.config.get("on_exceeded")
            && !matches!(policy.as_str(), Some("error") | Some("continue"))
        {
            errors.push(ValidationError::InvalidNodeConfig {
                node: node.id.clone(),
                reason: format!(
                    "loop `on_exceeded` must be \"error\" or \"continue\", got {policy}"
                ),
            });
        }
        // A body must both start and return to its loop head. Merely wiring a
        // body edge otherwise runs it once and silently strands the `done` path.
        let body_returns = graph
            .edges
            .iter()
            .filter(|e| e.from_node == node.id && e.from_port == "body")
            .any(|edge| path_exists(graph, &edge.to_node, &node.id));
        if !body_returns {
            errors.push(ValidationError::InvalidNodeConfig {
                node: node.id.clone(),
                reason: "loop node's `body` does not route back to the loop head, so it can never \
                         iterate; wire `body` to the first node of the loop body and wire that \
                         body's last node back to this loop"
                    .to_string(),
            });
        }
    }

    if loop_edges.is_empty() {
        return;
    }

    // Every node that sits on some cycle.
    let on_a_cycle: HashSet<&str> = loop_edges
        .iter()
        .flat_map(|(from, to)| nodes_on_cycle(graph, to, from))
        .collect();

    // A real fan-in `merge` inside the loop body deadlocks it. A single-input
    // merge is a passthrough and is not lowered as a waiting barrier.
    //
    // The rule is narrower than "any fan-in merge on a cycle", and both halves
    // of it are load bearing.
    //
    // Why an all-on-the-cycle merge is fine: barrier arrivals refill. The
    // arrival set is *removed* when the barrier fires (see
    // `graph::compiled::routing::route_completed`), so it re-arms for the next
    // pass. Every predecessor on the cycle runs again on every pass, so the set
    // completes again on every pass and the merge fires once per iteration.
    //
    // Why an off-cycle predecessor still hangs: it runs once, on the seeding
    // pass, and never activates again. From the second iteration the required
    // set can never be completed and the loop stops dead at the merge.
    //
    // This lift also depended on a fix elsewhere, worth recording because the
    // symptom pointed away from the cause. Loop-body arms are reachable only
    // through the head's `body` port, so relief is registered for them; relief
    // decides whether a branch was taken by walking forward through
    // deterministic routing, and that walk used to stop at a fan-out (a command
    // node has no static edge). It therefore concluded the arms were untaken
    // and injected phantom arrivals, firing the merge *before* its arms ran —
    // activation order `head, apex, join, arm_a, arm_b, …`, the join reading the
    // previous pass's data. The walk now crosses unconditional fan-outs, which
    // is what makes a diamond in a loop body correct rather than merely legal.
    for id in &on_a_cycle {
        let is_merge = graph
            .nodes
            .iter()
            .any(|n| n.id == *id && n.kind == NodeKind::Merge);
        if !is_merge {
            continue;
        }
        let forward_predecessors: Vec<&str> = graph
            .edges
            .iter()
            .filter(|edge| {
                edge.to_node == **id
                    && !loop_edges.contains(&(edge.from_node.clone(), edge.to_node.clone()))
            })
            .map(|edge| edge.from_node.as_str())
            .collect();
        let waits_on_something_off_the_cycle = forward_predecessors
            .iter()
            .any(|pred| !on_a_cycle.contains(pred));
        if forward_predecessors.len() > 1 && waits_on_something_off_the_cycle {
            errors.push(ValidationError::IllegalCycle((*id).to_string()));
        }
    }

    // A loop head that is *also* a fan-in used to be refused here.
    //
    // It was refused because the barrier gate is keyed on the target node
    // rather than on the edge, so the re-entry a back-edge delivered was tested
    // against the head's forward predecessors, failed, and was dropped — the
    // loop ran once and stopped. The gate now ignores arrivals from
    // predecessors outside the barrier's required set (see
    // `graph::compiled::routing::route_completed`), and a back-edge's source is
    // never in that set, so the re-entry lands and the loop iterates.
    //
    // Joining before the head — a `merge` outside the cycle — is still the
    // clearer way to write it, and is what the catalog recommends. It is no
    // longer the only way that works.

    // An unbounded cycle. Without a `loop` node to count passes, the only thing
    // standing between this graph and a run that spins until the host's wall
    // clock kills it is the trigger's `recursion_limit`. Requiring one of the
    // two makes the bound an authoring decision rather than an accident.
    // Either run-level bound counts. `max_node_visits` is enforced just as
    // firmly as `recursion_limit` and gives the *better* failure — it names the
    // node that ran away, where `recursion_limit` can only say the run did — so
    // refusing a graph bounded solely by it was refusing a graph that was in
    // fact bounded, and pushing authors toward the less informative knob.
    let has_run_level_bound = graph.trigger().is_some_and(|trigger| {
        ["recursion_limit", "max_node_visits"].iter().any(|key| {
            trigger
                .config
                .get(*key)
                .and_then(Value::as_u64)
                .is_some_and(|n| n > 0)
        })
    });
    if !has_run_level_bound {
        for (from, to) in &loop_edges {
            let bounded = nodes_on_cycle(graph, to, from).into_iter().any(|id| {
                graph
                    .nodes
                    .iter()
                    .any(|n| n.id == id && n.kind == NodeKind::Loop)
            });
            if !bounded {
                errors.push(ValidationError::IllegalCycle(to.clone()));
            }
        }
    }
}
