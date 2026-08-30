use super::*;

pub(super) fn wire_graph(
    mut builder: GraphBuilder<Value, Value>,
    graph: &crate::model::WorkflowGraph,
    trigger_id: &str,
    loop_edges: &std::collections::HashSet<(String, String)>,
) -> GraphBuilder<Value, Value> {
    let is_back_edge = |edge: &crate::model::Edge| {
        loop_edges.contains(&(edge.from_node.clone(), edge.to_node.clone()))
    };
    let mut incoming_counts: std::collections::HashMap<&str, usize> =
        std::collections::HashMap::new();
    for edge in graph.edges.iter().filter(|edge| !is_back_edge(edge)) {
        *incoming_counts.entry(edge.to_node.as_str()).or_default() += 1;
    }

    builder = builder.set_entry(trigger_id.to_string());
    for node in &graph.nodes {
        // Permit the interrupt at every approval-gate node so the engine can
        // pause there (the gate emits the interrupt from its handler above).
        if node
            .config
            .get("requires_approval")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            builder = builder.mark_interrupt(node.id.clone());
        }
        let outgoing: Vec<_> = graph
            .edges
            .iter()
            .filter(|e| e.from_node == node.id)
            .collect();
        if outgoing.is_empty() {
            // Leaf node: nothing routes out, so it terminates the run.
            builder = builder.add_edge(node.id.clone(), END);
            continue;
        }
        match handler_routing(graph, &node.id) {
            HandlerRouting::FanOut(dests) => {
                // Parallel fan-out: the node's handler drives every successor with
                // a `Command::goto`, so we only declare the destinations here.
                // A command-routing node may not also carry static/conditional
                // edges, so nothing else is wired for it.
                //
                // Declared as *unconditional*, which is a promise the routing
                // layer relies on rather than a hint: all of these successors run
                // whenever this node runs (they share one port — that is what
                // makes this a fan-out rather than a choice). Barrier relief
                // walks through this node on the strength of it, and would
                // otherwise treat the fan-out as an unresolvable decision, decide
                // a branch behind it went untaken, and clear a downstream barrier
                // early.
                builder = builder.with_unconditional_fanout(node.id.clone(), dests);
            }
            HandlerRouting::PortCommand(groups) => {
                // Mixed-port node (e.g. `main->a, main->b, error->h`): the handler
                // drives the emitted port's successors via `Command::goto`, so
                // declare the full destination set (union across ports) as hints.
                // This keeps every same-port successor (both `a` and `b`) instead
                // of the conditional-edge route map dropping the duplicate label.
                let dests: Vec<String> = groups
                    .into_iter()
                    .flat_map(|(_, targets)| targets)
                    .collect();
                builder = builder.with_command_destinations(node.id.clone(), dests);
            }
            HandlerRouting::Plain => {
                if let [edge] = outgoing.as_slice() {
                    let target = edge.to_node.clone();
                    if edge.from_port != "main" {
                        // A single outgoing edge whose port is NOT the default
                        // `main` (e.g. a `condition` wired with only a `true`
                        // edge, no `false` edge) is not a plain pass-through: the
                        // node still records which port it emitted on (see
                        // `items_update`), and `collect_input` port-matches an
                        // edge's `from_port` against that emitted port before
                        // handing items to the successor. Wiring this as an
                        // unconditional `add_edge` (the old behavior) made the
                        // successor run on *every* execution — including when the
                        // node emitted the other port — but with an EMPTY input,
                        // since `collect_input` silently drops the mismatched
                        // items. Downstream `=item`/`=item.<field>` expressions
                        // then resolve to `null` instead of the run simply ending.
                        //
                        // Fix: lower it as a conditional edge (mirroring the
                        // multi-edge branch below) that only follows `target` when
                        // the emitted port matches `edge.from_port`, and falls
                        // back to `END` otherwise. A `true`-only condition thus
                        // behaves as a FILTER — it runs the successor (with
                        // items) on `true`, and terminates the run to `END` on
                        // `false` — instead of a leaky always-on edge.
                        let from = node.id.clone();
                        let from_port = edge.from_port.clone();
                        builder = builder.add_conditional_edges(
                            node.id.clone(),
                            move |state: &Value| -> String {
                                let emitted = state
                                    .get("nodes")
                                    .and_then(|nodes| nodes.get(&from))
                                    .and_then(|slot| slot.get("port"))
                                    .and_then(Value::as_str)
                                    .unwrap_or("main");
                                if emitted == from_port.as_str() {
                                    from_port.clone()
                                } else {
                                    END.to_string()
                                }
                            },
                            [
                                (edge.from_port.clone(), target),
                                (END.to_string(), END.to_string()),
                            ],
                        );
                    } else {
                        // Single successor on the default `main` port. If the
                        // target is a fan-in point (more than one predecessor,
                        // e.g. a `merge`) it gets a waiting edge so it runs only
                        // once every predecessor completed — the merge barrier.
                        //
                        // Every fan-in edge is wired as waiting unconditionally,
                        // even when some predecessors are mutually exclusive
                        // conditional branches (a *conditional join*): lowering a
                        // conditional predecessor's edge as *plain* instead would
                        // let a *taken* branch's downstream node race past the
                        // barrier — the join can fire off the superstep snapshot
                        // *before* the conditional branch's items are committed,
                        // silently dropping them. Any fan-in with a conditional
                        // predecessor instead gets a barrier-relief registration
                        // (see below) so the all-waiting barrier still clears when
                        // that predecessor's branch is never taken, without ever
                        // weakening the barrier itself.
                        //
                        // A back-edge is never lowered as waiting: it is the
                        // loop head's re-entry, not a predecessor it waits for,
                        // and hard-waiting on it deadlocks the loop before its
                        // first iteration (`incoming_counts` already excludes
                        // it, so this only matters for the edge itself).
                        let is_fan_in = !is_back_edge(edge)
                            && incoming_counts
                                .get(edge.to_node.as_str())
                                .copied()
                                .unwrap_or(0)
                                > 1;
                        if is_fan_in {
                            builder = builder.add_waiting_edge(node.id.clone(), target);
                        } else {
                            builder = builder.add_edge(node.id.clone(), target);
                        }
                    }
                } else {
                    // Branching: distinct ports (one target each) lower to
                    // conditional edges keyed on the port the node recorded into
                    // state (defaulting to `main`).
                    let from = node.id.clone();
                    let routes: Vec<(String, String)> = outgoing
                        .iter()
                        .map(|e| (e.from_port.clone(), e.to_node.clone()))
                        .collect();
                    builder = builder.add_conditional_edges(
                        node.id.clone(),
                        move |state: &Value| -> String {
                            state
                                .get("nodes")
                                .and_then(|nodes| nodes.get(&from))
                                .and_then(|slot| slot.get("port"))
                                .and_then(Value::as_str)
                                .unwrap_or("main")
                                .to_string()
                        },
                        routes,
                    );
                }
            }
        }
    }

    // Mixed fan-in barrier relief. Every fan-in node above was wired with
    // all-waiting edges, even when one or more of its predecessors sit behind
    // a conditional branch that may never be taken — hard-waiting on all of
    // them is the only way to avoid the taken-branch data race described
    // above, but it means a *mixed* fan-in (one unconditionally-reachable
    // predecessor plus one reachable only via a conditional branch) would
    // deadlock forever on the branch that was never taken. For each such
    // conditional predecessor, register a relief against the brancher whose
    // port decides its fate, so the barrier can still clear on its remaining
    // predecessors when that branch goes untaken.
    for merge in &graph.nodes {
        if incoming_counts.get(merge.id.as_str()).copied().unwrap_or(0) <= 1 {
            continue;
        }
        for predecessor in conditional_predecessors(graph, &merge.id, loop_edges) {
            if let Some(brancher) = find_conditional_brancher(graph, &predecessor, &merge.id) {
                builder = builder.add_barrier_relief(brancher, predecessor, merge.id.clone());
            }
        }
    }
    builder
}
