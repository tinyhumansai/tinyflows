//! Structural validation for [`WorkflowGraph`]s, run before compilation.

use std::collections::HashSet;

use serde_json::Value;

use crate::error::ValidationError;
use crate::model::{NodeKind, WorkflowGraph, is_valid_input_name};

/// The node kind's wire discriminator (`tool_call`, `sub_workflow`, …) for use
/// in error messages, so a validation error names the kind the way the graph
/// JSON spells it rather than in Rust's `PascalCase`.
fn kind_name(kind: &NodeKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("{kind:?}"))
}

/// Validates a workflow graph's structure.
///
/// Currently checks: unique node ids, exactly one trigger node, that every edge
/// references existing nodes, no duplicate edges, per-node `on_error` policy
/// sanity (a known value, and an `error` edge when the policy is `route`),
/// `void` topology (a terminal sink may have no outgoing edge, and must have an
/// incoming one), declared-input sanity (addressable, unique names; defaults
/// that match their declared type), and loop legality (see [`validate_loops`] — cycles are
/// permitted; only the ones that cannot iterate are refused).
///
/// # Errors
/// Returns the first [`ValidationError`] encountered. For a full list of every
/// structural problem in one pass (so an author can fix them all at once
/// instead of one round-trip per error), use [`validate_all`]; this function is
/// exactly its first element and is kept for the fail-fast compile path.
pub fn validate(graph: &WorkflowGraph) -> Result<(), ValidationError> {
    match validate_all(graph).into_iter().next() {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// Validates a workflow graph's structure, collecting **every** independent
/// error in one pass.
///
/// Returns an empty `Vec` for a valid graph. The checks are ordered
/// deterministically (duplicate ids → trigger count → edge integrity →
/// `on_error` policy → per-kind config → `void` topology → condition routing →
/// declared inputs),
/// and every error is self-contained (no check can panic on a graph that failed
/// an earlier one), so accumulating is safe. The first element is identical to what
/// [`validate`] returns, preserving the historical single-error contract.
///
/// This is what a host should surface to an author or agent: fixing five
/// problems then costs one validate call, not five.
pub fn validate_all(graph: &WorkflowGraph) -> Vec<ValidationError> {
    let mut errors = Vec::new();

    let mut seen = HashSet::new();
    for node in &graph.nodes {
        if !seen.insert(node.id.as_str()) {
            errors.push(ValidationError::DuplicateNodeId(node.id.clone()));
        }
    }

    let triggers: Vec<String> = graph
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Trigger)
        .map(|n| n.id.clone())
        .collect();
    match triggers.len() {
        0 => errors.push(ValidationError::MissingTrigger),
        1 => {}
        _ => errors.push(ValidationError::MultipleTriggers(triggers)),
    }

    let mut seen_edges = HashSet::new();
    for edge in &graph.edges {
        if graph.node(&edge.from_node).is_none() {
            errors.push(ValidationError::UnknownNode(edge.from_node.clone()));
        }
        if graph.node(&edge.to_node).is_none() {
            errors.push(ValidationError::UnknownNode(edge.to_node.clone()));
        }
        // Reject two identical edges (same source node/port and destination
        // node/port); a redundant duplicate is almost always an authoring slip.
        if !seen_edges.insert((
            edge.from_node.as_str(),
            edge.from_port.as_str(),
            edge.to_node.as_str(),
            edge.to_port.as_str(),
        )) {
            errors.push(ValidationError::DuplicateEdge {
                from_node: edge.from_node.clone(),
                from_port: edge.from_port.clone(),
                to_node: edge.to_node.clone(),
                to_port: edge.to_port.clone(),
            });
        }
    }

    // Per-node `on_error` policy checks. The policy is free-form config read at
    // run time; catch mistakes at author time: an unknown value (which would
    // silently fall through to `stop`) and a `route` policy with no `error`
    // edge to carry the routed item (which would be silently dropped).
    for node in &graph.nodes {
        let Some(on_error) = node.config.get("on_error").and_then(Value::as_str) else {
            continue;
        };
        match on_error {
            "stop" | "continue" => {}
            "route" => {
                if node.kind == NodeKind::Void {
                    // An `error` edge is still an outgoing edge, which the
                    // `void` check below forbids. Caught here instead of
                    // letting `MissingErrorRoute` fire, or the author would be
                    // told to add an edge that the next rule then rejects —
                    // advice with no fixed point.
                    errors.push(ValidationError::InvalidNodeConfig {
                        node: node.id.clone(),
                        reason: "`void` is a terminal sink, so on_error \"route\" has nowhere to \
                                 route to (an `error` edge is an outgoing edge); use \"stop\" or \
                                 \"continue\""
                            .to_string(),
                    });
                } else {
                    let has_error_edge = graph
                        .edges
                        .iter()
                        .any(|e| e.from_node == node.id && e.from_port == "error");
                    if !has_error_edge {
                        errors.push(ValidationError::MissingErrorRoute(node.id.clone()));
                    }
                }
            }
            other => {
                errors.push(ValidationError::InvalidOnError {
                    node: node.id.clone(),
                    value: other.to_string(),
                });
            }
        }
    }

    validate_node_configs(graph, &mut errors);

    // `void` node topology checks. The kind asserts exactly one thing — "the
    // branch ends here, deliberately" — so the two ways to contradict it are
    // refused rather than absorbed. An outgoing edge would be dead (a leaf
    // lowers to the engine's `END` sentinel) or would make the node not a void;
    // and a void nothing routes into declares nothing at all, since a node with
    // no effect and no input is the one orphan that cannot be work in progress.
    // There is no general orphan check in this crate, and adding one is out of
    // scope; this rule is safe precisely because the kind is new, so no
    // existing graph can trip it.
    for node in &graph.nodes {
        if node.kind != NodeKind::Void {
            continue;
        }
        let mut outgoing: Vec<&str> = graph
            .edges
            .iter()
            .filter(|e| e.from_node == node.id)
            .map(|e| e.to_node.as_str())
            .collect();
        outgoing.sort_unstable();
        outgoing.dedup();
        if !outgoing.is_empty() {
            errors.push(ValidationError::InvalidNodeConfig {
                node: node.id.clone(),
                reason: format!(
                    "`void` is a terminal sink and may not have outgoing edges (found \
                     {outgoing:?}); remove the edge, or use a different kind if the branch is \
                     meant to continue"
                ),
            });
        }
        if !graph.edges.iter().any(|e| e.to_node == node.id) {
            errors.push(ValidationError::InvalidNodeConfig {
                node: node.id.clone(),
                reason: "`void` has no incoming edge, so it can never run and declares nothing; \
                         wire the branch it is meant to terminate, or delete it"
                    .to_string(),
            });
        }
    }

    // A `condition` node's outgoing edges must emit on `from_port` "true" or
    // "false" — routing is keyed EXCLUSIVELY on `from_port` (see
    // `engine::outgoing_by_port` / `handler_routing`), so any other value
    // (most commonly the default `"main"`, from an authoring mistake that put
    // the branch label on `to_port` instead) is a hard authoring bug: it
    // silently degrades to a parallel `FanOut` that drives BOTH branches
    // unconditionally, with no runtime error or warning to point at the
    // mistake. Caught here, at the door, instead.
    let condition_node_ids: HashSet<&str> = graph
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Condition)
        .map(|n| n.id.as_str())
        .collect();
    for edge in &graph.edges {
        if condition_node_ids.contains(edge.from_node.as_str())
            && edge.from_port != "true"
            && edge.from_port != "false"
        {
            errors.push(ValidationError::InvalidConditionRouting {
                node: edge.from_node.clone(),
                from_port: edge.from_port.clone(),
            });
        }
    }

    validate_loops(graph, &mut errors);
    validate_scatter_regions(graph, &mut errors);
    validate_working_dirs(graph, &mut errors);
    // Declared-input checks. These are author-time mistakes that would otherwise
    // surface as a confusing runtime `null`: a name that `=inputs.<name>` cannot
    // address, two declarations racing for the same key, a default the input's
    // own type would reject, or `required` alongside a default (which makes the
    // requirement unreachable — a default always supplies a value).
    let mut seen_inputs = HashSet::new();
    for input in &graph.inputs {
        if !is_valid_input_name(&input.name) {
            errors.push(ValidationError::InvalidInputName(input.name.clone()));
        }
        if !seen_inputs.insert(input.name.as_str()) {
            errors.push(ValidationError::DuplicateInputName(input.name.clone()));
        }
        match &input.default {
            Some(default) if !input.ty.accepts(default) => {
                errors.push(ValidationError::InputDefaultTypeMismatch {
                    name: input.name.clone(),
                    expected: input.ty.as_str(),
                });
            }
            Some(_) if input.required => {
                errors.push(ValidationError::RequiredInputWithDefault(
                    input.name.clone(),
                ));
            }
            _ => {}
        }
    }

    validate_agents(graph, &mut errors);

    errors
}

mod agents;
pub use agents::unresolved_agent_refs;
use agents::validate_agents;

mod loops;
use loops::validate_loops;

mod node_config;
use node_config::validate_node_configs;

mod scatter;
use scatter::{nodes_on_cycle, path_exists, validate_scatter_regions};

mod workdir;
use workdir::validate_working_dirs;

#[cfg(test)]
#[path = "validate_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "fanout_tests.rs"]
mod fanout_tests;

#[cfg(test)]
#[path = "loop_tests.rs"]
mod loop_tests;
