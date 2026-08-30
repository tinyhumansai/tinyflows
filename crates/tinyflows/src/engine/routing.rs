use super::*;

/// How a node's handler drives its successors once it has produced an update.
///
/// Most nodes follow their static/conditional edges (`Plain`). A node whose
/// outgoing edges all share one port and number two or more is a parallel
/// `FanOut` — it drives every successor with a single `Command::goto`. A node
/// with **mixed** ports where at least one port has more than one target is a
/// `PortCommand`: it drives only the successors of the port it actually emitted
/// on (so `main->a, main->b, error->h` fans out over `a`+`b` on success and
/// routes to `h` on error, instead of one `main` branch being dropped).

#[derive(Clone)]
pub(super) enum HandlerRouting {
    /// Follow static/conditional edges; emit a plain state update.
    Plain,
    /// Parallel fan-out: `goto` every listed successor regardless of port.
    FanOut(Vec<String>),
    /// Port-selective command routing: `goto` the successors of the emitted
    /// port, looked up in this `(port, targets)` table.
    PortCommand(Vec<(String, Vec<String>)>),
}

/// Groups a node's outgoing edges by `from_port`, preserving first-seen order of
/// both ports and their targets.
pub(super) fn outgoing_by_port(
    graph: &crate::model::WorkflowGraph,
    node_id: &str,
) -> Vec<(String, Vec<String>)> {
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    for edge in graph.edges.iter().filter(|e| e.from_node == node_id) {
        if let Some((_, targets)) = groups.iter_mut().find(|(port, _)| *port == edge.from_port) {
            targets.push(edge.to_node.clone());
        } else {
            groups.push((edge.from_port.clone(), vec![edge.to_node.clone()]));
        }
    }
    groups
}

/// Classifies how a node drives its successors from its outgoing-edge shape:
/// same-port multi-edge → parallel [`HandlerRouting::FanOut`]; mixed ports with a
/// multi-target port → [`HandlerRouting::PortCommand`]; everything else (leaf,
/// single edge, or one-target-per-port conditional) follows edges as `Plain`.
pub(super) fn handler_routing(
    graph: &crate::model::WorkflowGraph,
    node_id: &str,
) -> HandlerRouting {
    let groups = outgoing_by_port(graph, node_id);
    let total: usize = groups.iter().map(|(_, targets)| targets.len()).sum();
    match groups.len() {
        // Leaf or single successor: plain edge routing.
        0 => HandlerRouting::Plain,
        1 => {
            let targets = &groups[0].1;
            if targets.len() >= 2 {
                HandlerRouting::FanOut(targets.clone())
            } else {
                HandlerRouting::Plain
            }
        }
        // Multiple distinct ports. One target per port is a plain conditional
        // branch (lowered to conditional edges). If any port has >=2 targets the
        // conditional-edge route map would overwrite the duplicate label, so
        // drive it by the emitted port instead.
        _ if total == groups.len() => HandlerRouting::Plain,
        _ => HandlerRouting::PortCommand(groups),
    }
}

/// Whether `to` is reachable from `from` by following only edges on the
/// default `"main"` port, without ever expanding through `stop`.
///
/// This is the "always runs" reachability test: a node reached this way runs
/// on *every* execution (plain pass-throughs and parallel fan-outs alike,
/// since a `FanOut`/`PortCommand` node's declared successors all still sit on
/// a single shared port in `graph.edges`), whereas a node only reachable by
/// stepping onto a non-`main` port (a `true`/`false`/custom branch port) may
/// legitimately never run in a given execution — its brancher chose a
/// different port. `stop` mirrors [`reaches_via_port`]'s parameter: it keeps
/// the search from walking past a fan-in join point.
pub(super) fn has_unconditional_path(
    graph: &crate::model::WorkflowGraph,
    from: &str,
    to: &str,
    stop: &str,
) -> bool {
    if from == to {
        return true;
    }
    let mut stack: Vec<&str> = graph
        .edges
        .iter()
        .filter(|e| e.from_node == from && e.from_port == "main")
        .map(|e| e.to_node.as_str())
        .collect();
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    while let Some(node) = stack.pop() {
        if node == to {
            return true;
        }
        if node == stop || !seen.insert(node) {
            continue;
        }
        stack.extend(
            graph
                .edges
                .iter()
                .filter(|e| e.from_node == node && e.from_port == "main")
                .map(|e| e.to_node.as_str()),
        );
    }
    false
}

/// The direct predecessors of the fan-in node `merge_id` that are **not**
/// reachable from the workflow's trigger via an unconditional (`"main"`-only)
/// path — i.e. predecessors that sit behind a conditional/branch port and so
/// may legitimately never run in a given execution.
///
/// Every incoming edge of a fan-in node is lowered as a waiting edge (see the
/// edge-lowering loop below), so a fan-in with any conditional predecessor
/// needs a [`GraphBuilder::add_barrier_relief`] registration for it or its
/// barrier would wait forever on the branch that was never taken. A fan-in
/// whose predecessors are all unconditionally reachable (a plain diamond or a
/// parallel fan-out join) returns an empty set — those still hard-wait on
/// every predecessor, which is correct because every predecessor always runs.
///
/// Returns an empty set when the graph has no unique trigger (defensive; a
/// validated workflow always has exactly one).
///
/// `loop_edges` are the graph's back-edges (see [`back_edges`]) and are skipped
/// outright: a back-edge predecessor is the loop head's own re-entry, never a
/// branch that might go untaken, so registering a relief for it would be
/// meaningless — and its edge is not lowered as waiting in the first place.
pub(super) fn conditional_predecessors(
    graph: &crate::model::WorkflowGraph,
    merge_id: &str,
    loop_edges: &std::collections::HashSet<(String, String)>,
) -> std::collections::HashSet<String> {
    let Some(trigger) = graph.trigger() else {
        return std::collections::HashSet::new();
    };
    graph
        .edges
        .iter()
        .filter(|e| e.to_node == merge_id)
        .filter(|e| !loop_edges.contains(&(e.from_node.clone(), e.to_node.clone())))
        .map(|e| e.from_node.clone())
        .filter(|pred| !has_unconditional_path(graph, &trigger.id, pred, merge_id))
        .collect()
}

/// The graph's **back-edges**: the edges that close a cycle, as
/// `(from_node, to_node)` pairs.
///
/// Found with the standard grey-node test — a depth-first walk from the trigger
/// in which an edge pointing at a node still on the current DFS stack is, by
/// definition, an edge back to one of its own ancestors. Edges unreachable from
/// the trigger are walked afterwards so a detached loop is still classified
/// (it cannot run, but it must not be mistaken for a forward edge either).
///
/// This is load-bearing for loops rather than diagnostic. The edge-lowering
/// below treats a node with more than one predecessor as a fan-in point and
/// wires its incoming edges as *waiting* edges — the merge barrier. A loop head
/// (`trigger -> a -> b -> a`) has two predecessors by that count, so without
/// this distinction it would barrier on its own back-edge and wait forever for
/// an activation that can only happen after it runs. Excluding back-edges from
/// the fan-in count is what lets a loop actually iterate; a node that is both a
/// genuine fan-in *and* a loop head still barriers on its forward predecessors,
/// which is the correct semantics.
///
/// Because a cycle has no canonical entry point, *which* edge is reported as
/// the back-edge depends on node/edge order for graphs with several cycles
/// through the same nodes. That is fine for every use here: what matters is
/// that exactly one edge per cycle is cut, not which one.
///
/// Public because a host that mirrors the engine's fan-in classification — to
/// pre-flight a graph, lay it out, or explain it — must make the same
/// distinction, and a second implementation would drift from this one and
/// disagree about which graphs are legal.
pub fn back_edges(
    graph: &crate::model::WorkflowGraph,
) -> std::collections::HashSet<(String, String)> {
    /// DFS colours. `Grey` = on the current stack (an ancestor of the node
    /// being expanded), `Black` = fully explored.
    #[derive(Clone, Copy, PartialEq)]
    enum Colour {
        Grey,
        Black,
    }

    /// One iterative DFS from `root`, colouring as it goes and recording every
    /// edge it finds pointing at a grey (on-stack) node. Iterative rather than
    /// recursive so a deep graph cannot blow the stack; each frame carries the
    /// node's outgoing edges and how many have been expanded.
    fn visit<'g>(
        graph: &'g crate::model::WorkflowGraph,
        root: &'g str,
        colours: &mut std::collections::HashMap<&'g str, Colour>,
        found: &mut std::collections::HashSet<(String, String)>,
    ) {
        if colours.contains_key(root) {
            return;
        }
        let outgoing = |node: &str| -> Vec<&'g crate::model::Edge> {
            graph.edges.iter().filter(|e| e.from_node == node).collect()
        };
        colours.insert(root, Colour::Grey);
        let mut stack: Vec<(&'g str, Vec<&'g crate::model::Edge>, usize)> =
            vec![(root, outgoing(root), 0)];
        while let Some((node, edges, cursor)) = stack.last_mut() {
            if *cursor >= edges.len() {
                colours.insert(node, Colour::Black);
                stack.pop();
                continue;
            }
            let edge = edges[*cursor];
            *cursor += 1;
            // Resolve the target to a borrow that lives as long as `graph`, so
            // it can key the colour map. An edge pointing at a node that does
            // not exist is a validation error, not this pass's problem.
            let Some(target) = graph
                .nodes
                .iter()
                .find(|n| n.id == edge.to_node)
                .map(|n| n.id.as_str())
            else {
                continue;
            };
            match colours.get(target) {
                // Still on the stack: this edge points at an ancestor.
                Some(Colour::Grey) => {
                    found.insert((edge.from_node.clone(), edge.to_node.clone()));
                }
                // Already fully explored: a forward or cross edge, not a cycle.
                Some(Colour::Black) => {}
                None => {
                    colours.insert(target, Colour::Grey);
                    let next = outgoing(target);
                    stack.push((target, next, 0));
                }
            }
        }
    }

    let mut colours: std::collections::HashMap<&str, Colour> = std::collections::HashMap::new();
    let mut found: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();

    // The trigger first, so the classification matches the order a run actually
    // takes: which edge of a cycle is called the back-edge depends on where the
    // walk enters it.
    if let Some(trigger) = graph.trigger() {
        visit(graph, &trigger.id, &mut colours, &mut found);
    }
    // Then anything the trigger cannot reach: a detached subgraph may still
    // contain a cycle, and leaving it uncoloured would let its back-edge be
    // lowered as a waiting edge.
    for node in &graph.nodes {
        visit(graph, &node.id, &mut colours, &mut found);
    }
    found
}

/// Finds a brancher node `B` and one of its (>=2) outgoing ports whose
/// routing decides whether `predecessor` runs, so a
/// [`GraphBuilder::add_barrier_relief`] can be registered against `B`'s own
/// completion — the exact activation that decides `predecessor`'s fate.
///
/// A node counts as a brancher only when it has two or more distinct
/// outgoing ports (mirrors the old `is_conditional_join`'s brancher
/// detection): a same-port `FanOut`/`PortCommand` node's successors all run
/// together and are never conditional. Returns the first match in graph node
/// order; when no brancher is found (defensive — should not happen for a
/// `conditional_predecessors` result on a validated graph) the caller skips
/// registering a relief, preserving the safe (if potentially
/// over-cautious/deadlocking) waiting-edge barrier rather than guessing.
pub(super) fn find_conditional_brancher(
    graph: &crate::model::WorkflowGraph,
    predecessor: &str,
    stop: &str,
) -> Option<String> {
    for brancher in &graph.nodes {
        let ports = outgoing_by_port(graph, &brancher.id);
        if ports.len() < 2 {
            continue;
        }
        if ports
            .iter()
            .any(|(port, _)| reaches_via_port(graph, &brancher.id, port, predecessor, stop))
        {
            return Some(brancher.id.clone());
        }
    }
    None
}

/// Whether `target` is reachable from `brancher`'s `port` successors, walking
/// forward along edges but never expanding `stop` (the join node), so paths that
/// only reconverge at the join are not counted.
pub(super) fn reaches_via_port(
    graph: &crate::model::WorkflowGraph,
    brancher: &str,
    port: &str,
    target: &str,
    stop: &str,
) -> bool {
    let mut stack: Vec<&str> = graph
        .edges
        .iter()
        .filter(|e| e.from_node == brancher && e.from_port == port)
        .map(|e| e.to_node.as_str())
        .collect();
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    while let Some(node) = stack.pop() {
        if node == target {
            return true;
        }
        if node == stop || !seen.insert(node) {
            continue;
        }
        for edge in graph.edges.iter().filter(|e| e.from_node == node) {
            stack.push(edge.to_node.as_str());
        }
    }
    false
}

/// Builds the partial state update a node contributes:
/// `{ "nodes": { id: { items, port } } }`. The output `port` is explicitly
/// cleared when the node uses the default route. This matters because the graph
/// reducer merges node slots key-by-key: omitting `port` would preserve a port
/// emitted by an earlier activation and could misroute loops after a node
/// recovers from an error.
pub(super) fn items_update(
    node_id: &str,
    items: &[Item],
    port: Option<&str>,
) -> crate::graph::Result<Value> {
    items_update_with_meta(node_id, items, port, None)
}

/// [`items_update`], plus any extra slot keys the executor asked to record via
/// [`NodeOutput::meta`] — a node that must remember something across its own
/// activations, currently only the `loop` node's `iteration` count.
///
/// Unlike `items`/`port`, meta keys are written only when supplied. That is
/// deliberate and the opposite of the `port` rule above: `port` is cleared so a
/// stale route cannot survive the key-by-key merge, whereas a counter's whole
/// purpose is to survive it. A node that emits no meta on some activation
/// therefore leaves the previous value standing rather than resetting it.
pub(super) fn items_update_with_meta(
    node_id: &str,
    items: &[Item],
    port: Option<&str>,
    meta: Option<&Value>,
) -> crate::graph::Result<Value> {
    let mut slot = Map::new();
    slot.insert("items".to_string(), serde_json::to_value(items)?);
    slot.insert(
        "port".to_string(),
        port.map_or(Value::Null, |port| Value::String(port.to_string())),
    );
    if let Some(Value::Object(meta)) = meta {
        for (key, value) in meta {
            slot.insert(key.clone(), value.clone());
        }
    }
    let mut nodes = Map::new();
    nodes.insert(node_id.to_string(), Value::Object(slot));
    let mut root = Map::new();
    root.insert("nodes".to_string(), Value::Object(nodes));
    Ok(Value::Object(root))
}

/// Builds the error item a node emits when its `on_error` policy is `continue` or
/// `route`, turning a failed execution into routable data rather than a run-ending
/// event: `{ "error": { "message", "node" } }`.
pub(super) fn error_item(node_id: &str, e: &EngineError) -> Item {
    Item::new(json!({ "error": { "message": e.to_string(), "node": node_id } }))
}
