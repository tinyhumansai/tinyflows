//! Topologies this engine's fan-in lowering cannot execute safely.
//!
//! [`validate`](crate::validate) answers "is this a well-formed graph" and
//! [`gates`](crate::gates) answers "are its bindings resolvable". Neither
//! answers the third question, which is about the engine rather than the graph:
//! *can this implementation actually run it?*
//!
//! One shape cannot be. Every fan-in edge is lowered as a waiting edge with a
//! barrier relief registered for conditional predecessors, and that relief picks
//! only the **first** upstream brancher. When a predecessor sits behind two
//! branching decisions, the relief either fires before the real predecessor ran
//! — silently dropping its data — or never fires and the fan-in hangs. Which of
//! the two happens depends on node declaration order, so the same graph can
//! behave differently between two saves of the same file.
//!
//! This module refuses those graphs, and it fails **closed**: until the lowering
//! models nested decisions directly, a graph it cannot prove safe is refused
//! rather than run. A predecessor reachable from the trigger by `main`-only
//! edges is unconditional and needs no relief, so it is never flagged.
//!
//! The classification deliberately mirrors the lowering rather than checking
//! `merge` nodes alone: *any* node with multiple incoming edges is lowered as a
//! fan-in barrier, so any of them can hit this.
//!
//! # Inline children are walked; saved ones are the host's
//!
//! An inline `sub_workflow` is part of the graph and is descended into, to the
//! same depth the run will use — one budget shared across the whole call chain,
//! which is why [`errors_with_max_depth`] exists at all. A `sub_workflow` that
//! names a *saved* workflow cannot be resolved here, because this crate has no
//! catalog; a host that has one walks those itself and passes the remaining
//! depth in.

use std::collections::HashSet;

use crate::model::{NodeKind, WorkflowGraph};

/// One refusal: a topology the engine cannot execute safely.
///
/// Carries a stable `code` so a host can map it onto its own validation error
/// shape without matching on prose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompatibilityError {
    /// Stable, machine-readable identifier for the refusal.
    pub code: &'static str,
    /// Human-readable description, naming the node and what to change.
    pub message: String,
    /// The node the refusal is anchored to, when it is node-specific.
    pub node_id: Option<String>,
}

/// A fan-in predecessor controlled by more than one branching decision.
pub const UNSUPPORTED_NESTED_CONDITIONAL_FAN_IN: &str = "unsupported_nested_conditional_fan_in";
/// A fan-in reached through a brancher's `main` port, where the relief cannot
/// tell which branch actually ran.
pub const UNSUPPORTED_MAIN_PORT_CONDITIONAL_FAN_IN: &str =
    "unsupported_main_port_conditional_fan_in";

/// Every unsafe topology in `graph`, including its inline `sub_workflow`
/// children, to the nesting depth the graph itself declares.
///
/// An empty result is a pass. See the module doc for what "unsafe" means here
/// and why it is refused rather than warned about.
#[must_use]
pub fn errors(graph: &WorkflowGraph) -> Vec<CompatibilityError> {
    errors_with_max_depth(graph, max_sub_workflow_depth(graph))
}

/// Same walk as [`errors`], but with the inline-nesting
/// budget passed in rather than recomputed from `graph`'s own trigger.
///
/// A host walking saved children needs this: a saved child
/// reached partway through the root's referenced-workflow chain must still be
/// checked to the *remaining* depth the root's own `max_sub_workflow_depth`
/// allows, not to the child's own (possibly lower/default) declared cap —
/// the engine's runtime depth counter is one budget shared across the whole
/// inline-plus-referenced call chain, so a fan-in the child's own cap would
/// not reach can still be reached from the root.
pub fn errors_with_max_depth(graph: &WorkflowGraph, max_depth: u64) -> Vec<CompatibilityError> {
    let mut errors = Vec::new();
    collect_errors(graph, 0, max_depth, &mut errors);
    errors
}

/// The nesting cap this graph declares on its trigger, or the engine default.
///
/// The static walk below has to descend as deep as the run actually will, or a
/// graph that legitimately nests past the default would stop being checked
/// exactly where it starts being interesting.
pub fn max_sub_workflow_depth(graph: &WorkflowGraph) -> u64 {
    graph
        .trigger()
        .and_then(|t| t.config.get("max_sub_workflow_depth"))
        .and_then(serde_json::Value::as_u64)
        .filter(|n| *n > 0)
        .unwrap_or(crate::engine::MAX_SUB_WORKFLOW_DEPTH)
}

fn collect_errors(
    graph: &WorkflowGraph,
    depth: u64,
    max_depth: u64,
    errors: &mut Vec<CompatibilityError>,
) {
    errors.extend(graph_errors(graph));
    if depth >= max_depth {
        return;
    }

    for node in &graph.nodes {
        if node.kind != NodeKind::SubWorkflow {
            continue;
        }
        let Some(inline) = node.config.get("workflow") else {
            continue;
        };
        let Ok(child) = serde_json::from_value::<WorkflowGraph>(inline.clone()) else {
            // A malformed inline child is reported as a capability error at run time;
            // this gate is specifically for otherwise-deserializable unsafe
            // topologies.
            continue;
        };
        let first_child_error = errors.len();
        collect_errors(&child, depth + 1, max_depth, errors);
        for error in &mut errors[first_child_error..] {
            error.message = format!("Inline sub_workflow node '{}': {}", node.id, error.message);
        }
    }
}

fn graph_errors(graph: &WorkflowGraph) -> Vec<CompatibilityError> {
    let Some(trigger) = graph.trigger() else {
        return Vec::new();
    };
    let mut errors = Vec::new();

    // The edges that close a cycle, from the engine's own classifier rather
    // than a second implementation here — this gate mirrors the fan-in
    // lowering, so the two must agree on which edges count. A back-edge is a
    // loop head's re-entry, not a predecessor it barriers on, and counting it
    // would report every legal loop as an unrelieved fan-in.
    let loop_edges = crate::engine::back_edges(graph);

    for fan_in in &graph.nodes {
        let incoming: Vec<&str> = graph
            .edges
            .iter()
            .filter(|edge| edge.to_node == fan_in.id)
            .filter(|edge| !loop_edges.contains(&(edge.from_node.clone(), edge.to_node.clone())))
            .map(|edge| edge.from_node.as_str())
            .collect();
        if incoming.len() <= 1 {
            continue;
        }

        for predecessor in incoming {
            // Reaching a router itself unconditionally does not make the edge
            // it selects into the fan-in unconditional. Let router
            // predecessors reach the port-aware analysis below.
            if !is_branching_node(graph, predecessor)
                && reaches_on_main_edges(graph, &trigger.id, predecessor, &fan_in.id)
            {
                continue;
            }

            let mut controlling_branchers = 0usize;
            let mut controlled_via_main_port = false;
            for candidate in &graph.nodes {
                let is_router = matches!(candidate.kind, NodeKind::Condition | NodeKind::Switch);
                let ports: HashSet<&str> = graph
                    .edges
                    .iter()
                    .filter(|edge| edge.from_node == candidate.id)
                    .map(|edge| edge.from_port.as_str())
                    .collect();
                if ports.len() < 2 && !is_router {
                    continue;
                }
                // When the router is itself the incoming predecessor, its
                // branch edge must be tested against the fan-in (asking whether
                // that edge reaches the router again can never succeed).
                let controlled_target = if candidate.id == predecessor {
                    fan_in.id.as_str()
                } else {
                    predecessor
                };
                let reaches_from_port = |port: &str| {
                    reaches_via_port(graph, &candidate.id, port, controlled_target, &fan_in.id)
                };
                let any_port_reaches = ports.iter().any(|port| reaches_from_port(port));
                // A router with one wired output still has unwired runtime
                // choices that emit no successor, so that sole edge cannot
                // prove unconditional reachability. Router reconvergence is
                // only deterministic when every runtime choice is wired:
                // both condition outcomes, or a switch fallback. Generic
                // multi-port nodes retain their existing all-port behavior.
                let routing_choices_are_exhaustive = match candidate.kind {
                    NodeKind::Condition => ports.contains("true") && ports.contains("false"),
                    NodeKind::Switch => ports.contains("default"),
                    _ => true,
                };
                let can_prove_all_routing_choices = if is_router {
                    routing_choices_are_exhaustive
                } else {
                    ports.len() >= 2
                };
                let every_port_deterministically_reaches = can_prove_all_routing_choices
                    && ports.iter().all(|port| {
                        reaches_deterministically_via_port(
                            graph,
                            &candidate.id,
                            port,
                            controlled_target,
                            &fan_in.id,
                        )
                    });
                // A multi-port node only controls this predecessor when the
                // predecessor is reachable from it but not guaranteed by a
                // deterministic path on every routing choice. This matches
                // TinyAgents' relief proof, which stops at another router.
                if any_port_reaches && !every_port_deterministically_reaches {
                    controlling_branchers += 1;
                    controlled_via_main_port |= ports.contains("main") && reaches_from_port("main");
                }
            }

            let (code, routing_kind) = if controlled_via_main_port {
                (
                    UNSUPPORTED_MAIN_PORT_CONDITIONAL_FAN_IN,
                    "a conditional branch labelled 'main'",
                )
            } else if controlling_branchers >= 2 {
                (
                    UNSUPPORTED_NESTED_CONDITIONAL_FAN_IN,
                    "nested conditional routing",
                )
            } else {
                continue;
            };
            errors.push(CompatibilityError {
                code,
                message: format!(
                    "Fan-in node '{}' has predecessor '{}' behind {routing_kind}; \
                     this topology is temporarily unsupported because it can silently lose \
                     merged data. Flatten the conditional branch or join it before this fan-in.",
                    fan_in.id, predecessor
                ),
                node_id: Some(fan_in.id.clone()),
            });
        }
    }

    errors
}

/// [`errors`], collapsed to the first refusal as a `code: message` string.
///
/// The convenience a save path wants: it has one place to put a rejection
/// reason, and reporting all of them would not change what the author does
/// next — the topology has to be flattened either way.
///
/// # Errors
///
/// The first [`CompatibilityError`], formatted as `code: message`.
pub fn ensure_compatible(graph: &WorkflowGraph) -> Result<(), String> {
    match errors(graph).into_iter().next() {
        Some(error) => Err(format!("{}: {}", error.code, error.message)),
        None => Ok(()),
    }
}

fn reaches_on_main_edges(graph: &WorkflowGraph, from: &str, to: &str, stop: &str) -> bool {
    if from == to {
        return true;
    }
    let mut stack: Vec<&str> = if is_branching_node(graph, from) {
        Vec::new()
    } else {
        graph
            .edges
            .iter()
            .filter(|edge| edge.from_node == from && edge.from_port == "main")
            .map(|edge| edge.to_node.as_str())
            .collect()
    };
    let mut seen = HashSet::new();
    while let Some(node) = stack.pop() {
        if node == to {
            return true;
        }
        if node == stop || !seen.insert(node) {
            continue;
        }
        // Port labels are arbitrary. A node with multiple distinct output
        // ports is runtime-selective even when one label happens to be `main`,
        // so nothing beyond it is unconditionally reachable.
        if is_branching_node(graph, node) {
            continue;
        }
        stack.extend(
            graph
                .edges
                .iter()
                .filter(|edge| edge.from_node == node && edge.from_port == "main")
                .map(|edge| edge.to_node.as_str()),
        );
    }
    false
}

fn is_branching_node(graph: &WorkflowGraph, node_id: &str) -> bool {
    graph.nodes.iter().any(|node| {
        node.id == node_id && matches!(node.kind, NodeKind::Condition | NodeKind::Switch)
    }) || graph
        .edges
        .iter()
        .filter(|edge| edge.from_node == node_id)
        .map(|edge| edge.from_port.as_str())
        .collect::<HashSet<_>>()
        .len()
        >= 2
}

fn reaches_via_port(
    graph: &WorkflowGraph,
    brancher: &str,
    port: &str,
    target: &str,
    stop: &str,
) -> bool {
    let mut stack: Vec<&str> = graph
        .edges
        .iter()
        .filter(|edge| edge.from_node == brancher && edge.from_port == port)
        .map(|edge| edge.to_node.as_str())
        .collect();
    let mut seen = HashSet::new();
    while let Some(node) = stack.pop() {
        if node == target {
            return true;
        }
        if node == stop || !seen.insert(node) {
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

fn reaches_deterministically_via_port(
    graph: &WorkflowGraph,
    brancher: &str,
    port: &str,
    target: &str,
    stop: &str,
) -> bool {
    graph
        .edges
        .iter()
        .filter(|edge| edge.from_node == brancher && edge.from_port == port)
        .any(|edge| reaches_on_main_edges(graph, &edge.to_node, target, stop))
}

#[cfg(test)]
#[path = "compat_tests.rs"]
mod tests;
