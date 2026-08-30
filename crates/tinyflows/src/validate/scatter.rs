use super::*;

/// Checks the structural rules a `scatter`/`gather` region has to satisfy.
///
/// A lane is created by routing, not by an edge, so most of what makes a region
/// work cannot be seen in the graph at run time — the engine just propagates a
/// lane envelope to every successor that is not a gather. These rules are what
/// keep that propagation *total*: if a lane can leak out of the region, or end
/// somewhere that is not a gather, the envelope is silently dropped and the
/// activation writes the node's top-level slot as though it were not in a lane
/// at all. That is a wrong answer rather than a failure, so it is refused here.
pub(super) fn validate_scatter_regions(graph: &WorkflowGraph, errors: &mut Vec<ValidationError>) {
    let scatters: Vec<&str> = graph
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Scatter)
        .map(|n| n.id.as_str())
        .collect();
    let gathers: HashSet<&str> = graph
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Gather)
        .map(|n| n.id.as_str())
        .collect();
    let voids: HashSet<&str> = graph
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Void)
        .map(|n| n.id.as_str())
        .collect();

    // A gather with no scatter waits on lanes nobody will ever open.
    for gather in &gathers {
        let reached_by_a_scatter = scatters
            .iter()
            .any(|scatter| path_exists(graph, scatter, gather));
        if !reached_by_a_scatter {
            errors.push(ValidationError::InvalidNodeConfig {
                node: (*gather).to_string(),
                reason: "gather is not downstream of any `scatter`, so no lane can ever reach \
                         it and the run would wait until its poll budget ran out"
                    .to_string(),
            });
        }
    }

    for scatter in &scatters {
        // Every path out of a scatter has to end at a gather. A lane that runs
        // off the end of the graph is work whose results nothing collects.
        let members = region_members(graph, scatter, &gathers);
        if members.is_empty() {
            errors.push(ValidationError::InvalidNodeConfig {
                node: (*scatter).to_string(),
                reason: "scatter has no `gather` downstream; every lane it opens would run with \
                         nothing to collect it. Wire the end of the lane body to a `gather`"
                    .to_string(),
            });
            continue;
        }

        for member in &members {
            // A nested scatter needs composed lane ids and a gather that knows
            // which level it closes. Refused rather than mis-collected.
            if scatters.contains(member) {
                errors.push(ValidationError::InvalidNodeConfig {
                    node: (*member).to_string(),
                    reason: format!(
                        "nested `scatter` inside the region opened by {scatter:?} is not \
                         supported"
                    ),
                });
            }
            // A loop head inside a lane: re-entry detection keys on the node's
            // top-level slot, which a lane activation deliberately never writes.
            if graph
                .nodes
                .iter()
                .any(|n| n.id == **member && n.kind == NodeKind::Loop)
            {
                errors.push(ValidationError::InvalidNodeConfig {
                    node: (*member).to_string(),
                    reason: format!(
                        "`loop` inside the lane body of {scatter:?} is not supported: loop \
                         re-entry is tracked in the node's own slot, which a lane does not write"
                    ),
                });
            }
            // An approval gate inside a lane: the resume map is keyed by node
            // id, so N lanes of one node would share a single approval.
            if graph.nodes.iter().any(|n| {
                n.id == **member
                    && n.config
                        .get("requires_approval")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
            }) {
                errors.push(ValidationError::InvalidNodeConfig {
                    node: (*member).to_string(),
                    reason: format!(
                        "`requires_approval` inside the lane body of {scatter:?} is not \
                         supported: a resume is addressed by node id, so every lane would share \
                         one approval"
                    ),
                });
            }
            // A lane that dead-ends: every node inside the region must have a
            // path onward to a gather. One that does not is running in a lane
            // whose results nothing collects — and because a lane activation
            // deliberately never writes the node's top-level slot, its output
            // is not merely uncollected, it is invisible. Wrong answer, not a
            // failure, which is why this is refused rather than warned about.
            //
            // A branch ending in `void` is exempt: the rule exists to catch
            // *accidental* invisibility, and a void is the author declaring it.
            // The side effects along that branch still happen once per lane;
            // only the data is dropped, which is what the node means. This does
            // not reopen the "scatter with no gather at all" hole — a region
            // whose walk never reaches a gather has no members (see
            // `region_members`) and is already refused above.
            let reaches_a_gather = gathers
                .iter()
                .any(|gather| path_exists(graph, member, gather));
            let reaches_a_void = voids.iter().any(|sink| path_exists(graph, member, sink));
            if !reaches_a_gather && !reaches_a_void {
                errors.push(ValidationError::InvalidNodeConfig {
                    node: (*member).to_string(),
                    reason: format!(
                        "node is inside the lane region opened by {scatter:?} but has no path \
                         onward to a `gather`, so its lane output would be stranded; route it \
                         through the gather"
                    ),
                });
            }
        }
    }
}

/// The nodes strictly between `scatter` and the gathers it reaches.
///
/// Forward reachability from the scatter, stopping at any gather — the gather
/// itself is the boundary, not a member, because it is the one node a lane
/// reaches as a plain activation rather than as a lane.
fn region_members<'a>(
    graph: &'a WorkflowGraph,
    scatter: &str,
    gathers: &HashSet<&str>,
) -> HashSet<&'a str> {
    let mut members = HashSet::new();
    let mut reached_a_gather = false;
    let mut stack: Vec<&str> = graph
        .edges
        .iter()
        .filter(|e| e.from_node == scatter)
        .map(|e| e.to_node.as_str())
        .collect();
    while let Some(node) = stack.pop() {
        if gathers.contains(node) {
            reached_a_gather = true;
            continue;
        }
        let Some(id) = graph
            .nodes
            .iter()
            .find(|n| n.id == node)
            .map(|n| n.id.as_str())
        else {
            continue;
        };
        if !members.insert(id) {
            continue;
        }
        stack.extend(
            graph
                .edges
                .iter()
                .filter(|e| e.from_node == node)
                .map(|e| e.to_node.as_str()),
        );
    }
    if reached_a_gather {
        members
    } else {
        HashSet::new()
    }
}

pub(super) fn path_exists(graph: &WorkflowGraph, start: &str, target: &str) -> bool {
    let mut seen = HashSet::new();
    let mut stack = vec![start];
    while let Some(node) = stack.pop() {
        if node == target {
            return true;
        }
        if !seen.insert(node) {
            continue;
        }
        stack.extend(
            graph
                .edges
                .iter()
                .filter(|edge| edge.from_node == node)
                .map(|edge| edge.to_node.as_str()),
        );
    }
    false
}

/// The nodes lying on the cycle closed by the back-edge `end -> start`: every
/// node reachable forward from `start` that can also still reach `end`.
///
/// Used to ask questions about a specific cycle ("is there a `merge` on it?",
/// "is there a `loop` node bounding it?") rather than about the graph at large,
/// so an unrelated `merge` elsewhere is never blamed for a loop's problem.
pub(super) fn nodes_on_cycle<'g>(
    graph: &'g WorkflowGraph,
    start: &str,
    end: &str,
) -> HashSet<&'g str> {
    // Forward reachability from `start`.
    let mut forward: HashSet<&str> = HashSet::new();
    let mut stack: Vec<&str> = vec![];
    if let Some(node) = graph.nodes.iter().find(|n| n.id == start) {
        forward.insert(node.id.as_str());
        stack.push(node.id.as_str());
    }
    while let Some(node) = stack.pop() {
        for edge in graph.edges.iter().filter(|e| e.from_node == node) {
            if let Some(target) = graph
                .nodes
                .iter()
                .find(|n| n.id == edge.to_node)
                .map(|n| n.id.as_str())
                && forward.insert(target)
            {
                stack.push(target);
            }
        }
    }

    // Of those, the ones that can still reach `end` — walking backwards from
    // `end` keeps the search inside the cycle.
    let mut backward: HashSet<&str> = HashSet::new();
    let mut stack: Vec<&str> = vec![];
    if let Some(node) = graph.nodes.iter().find(|n| n.id == end) {
        backward.insert(node.id.as_str());
        stack.push(node.id.as_str());
    }
    while let Some(node) = stack.pop() {
        for edge in graph.edges.iter().filter(|e| e.to_node == node) {
            if let Some(source) = graph
                .nodes
                .iter()
                .find(|n| n.id == edge.from_node)
                .map(|n| n.id.as_str())
                && backward.insert(source)
            {
                stack.push(source);
            }
        }
    }

    forward.intersection(&backward).copied().collect()
}
