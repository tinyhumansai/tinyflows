//! Whether a repaired graph may continue the run its parent broke, or has to
//! start over.
//!
//! A failed run leaves its prefix committed: the engine's failure boundary
//! holds everything that finished before the node that broke, so the next
//! attempt can re-enter at that node instead of redoing the lot. For a graph
//! with effects in it that is not an optimisation — re-running a step that
//! posted a comment posts a second one.
//!
//! But a repair produces a *different graph*, and the committed prefix was
//! produced by the old one. Continuing is only sound when the two agree about
//! everything that already ran. This module is that check, and it is
//! deliberately a mechanical one: no model is asked whether its own edit was
//! safe to skip work over.
//!
//! **The rule.** Continue only when no edit touches a node that is an ancestor
//! of the failed one — in *either* graph.
//!
//! Both graphs, because the two directions fail differently and each is
//! invisible from the other side. An edit that *removes* an upstream node is
//! only an ancestor in the parent. An edit that *adds* one — a fetch step
//! wired in ahead of the node that starved without it — is only an ancestor in
//! the child, and it is the more dangerous of the two: the new node has no
//! committed output, so continuing would re-enter the failed node with the
//! upstream it was just given still missing, and the fix would look like it
//! had not worked.
//!
//! Editing the failed node itself is fine, and is the case worth having: a
//! continue re-runs it, so its config is read fresh. So is editing anything
//! downstream — none of it has run.

use std::collections::{HashMap, HashSet};

use tinyflows::graph_ops::GraphOp;
use tinyflows::model::WorkflowGraph;

/// Whether the run that failed at `failed_node` may be continued under `child`.
///
/// `parent` is the graph that ran, `child` the repaired one, and `ops` the
/// edits between them. `false` is the safe answer and the default for anything
/// this cannot reason about — an unknown node id, an empty failed node.
///
/// A `false` here is not a refusal to retry. It means the retry starts from
/// the trigger, which is what every attempt did before continuing existed.
#[must_use]
pub fn may_continue(
    parent: &WorkflowGraph,
    child: &WorkflowGraph,
    failed_node: &str,
    ops: &[GraphOp],
) -> bool {
    if failed_node.is_empty() {
        return false;
    }
    // A node the run never reached cannot be where it stopped; something is
    // out of step, and guessing is the one thing this must not do.
    if !parent.nodes.iter().any(|node| node.id == failed_node) {
        return false;
    }
    let mut upstream = ancestors(parent, failed_node);
    upstream.extend(ancestors(child, failed_node));
    let Some(edited) = touched(ops) else {
        // A whole-graph edit invalidates every committed output at once. Still
        // safe when there is no prefix to invalidate — a node with no
        // ancestors has nothing committed ahead of it.
        return upstream.is_empty();
    };
    upstream.is_disjoint(&edited)
}

/// Every node that can reach `target` by following edges forward.
///
/// The committed prefix is exactly this set's output, so it is exactly the set
/// a continue takes on trust. Walks the reverse edges breadth-first; a cycle
/// terminates because a node already seen is not queued again.
fn ancestors(graph: &WorkflowGraph, target: &str) -> HashSet<String> {
    let mut incoming: HashMap<&str, Vec<&str>> = HashMap::new();
    for edge in &graph.edges {
        incoming
            .entry(edge.to_node.as_str())
            .or_default()
            .push(edge.from_node.as_str());
    }
    let mut seen: HashSet<String> = HashSet::new();
    let mut queue = vec![target];
    while let Some(node) = queue.pop() {
        for from in incoming.get(node).into_iter().flatten() {
            if seen.insert((*from).to_string()) {
                queue.push(from);
            }
        }
    }
    seen
}

/// Every node id an edit names, or `None` when an edit reaches all of them.
///
/// Both endpoints of an edge op, because an edge is a statement about two
/// nodes: rewiring what an ancestor feeds changes that ancestor's meaning as
/// much as editing its config does.
///
/// `None` is for an op that names no node and yet changes what every node
/// reads — today only `SetWorkflowInputs`. Returning it as an unmatchable
/// sentinel id was the first attempt and was silently wrong: a set containing
/// only that id is disjoint from every real ancestor set, so the op read as
/// touching *nothing*. The absence has to be in the type.
fn touched(ops: &[GraphOp]) -> Option<HashSet<String>> {
    let mut names: HashSet<String> = HashSet::new();
    for op in ops {
        match op {
            GraphOp::AddNode { node } => {
                names.insert(node.id.clone());
            }
            GraphOp::UpdateNodeConfig { id, .. }
            | GraphOp::SetNodeName { id, .. }
            | GraphOp::RemoveNode { id }
            | GraphOp::SetNodePosition { id, .. } => {
                names.insert(id.clone());
            }
            GraphOp::RenameNode { id, new_id } => {
                names.insert(id.clone());
                names.insert(new_id.clone());
            }
            GraphOp::AddEdge { edge } => {
                names.insert(edge.from_node.clone());
                names.insert(edge.to_node.clone());
            }
            GraphOp::RemoveEdge {
                from_node, to_node, ..
            } => {
                names.insert(from_node.clone());
                names.insert(to_node.clone());
            }
            // Declared inputs are read by `=`-expressions anywhere in the
            // graph, including in the prefix that already ran, so a change to
            // them invalidates every committed output at once.
            GraphOp::SetWorkflowInputs { .. } => return None,
        }
    }
    Some(names)
}

#[cfg(test)]
#[path = "resume_tests.rs"]
mod tests;
