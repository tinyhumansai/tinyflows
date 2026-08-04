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
/// declared-input sanity (addressable, unique names; defaults that match their
/// declared type), and loop legality (see [`validate_loops`] — cycles are
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
/// `on_error` policy → per-kind config → condition routing → declared inputs),
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
                let has_error_edge = graph
                    .edges
                    .iter()
                    .any(|e| e.from_node == node.id && e.from_port == "error");
                if !has_error_edge {
                    errors.push(ValidationError::MissingErrorRoute(node.id.clone()));
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

    // Per-kind config checks. A `sub_workflow` node must reference its child
    // exactly one way: an inline `workflow` graph OR a `workflow_id` reference,
    // never both and never neither (the reference form is resolved at run time
    // via the host `WorkflowResolver`).
    for node in &graph.nodes {
        if node.kind == NodeKind::SubWorkflow {
            let has_inline = node.config.get("workflow").is_some();
            let has_ref = node
                .config
                .get("workflow_id")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|s| !s.is_empty());
            if has_inline == has_ref {
                errors.push(ValidationError::InvalidNodeConfig {
                    node: node.id.clone(),
                    reason: "sub_workflow requires exactly one of `workflow` (inline) or \
                             `workflow_id` (reference)"
                        .to_string(),
                });
            }
        }
    }

    // Per-item fan-out config (`execution` / `concurrency` / `on_item_error`).
    // These select the execution strategy, so an unrecognized value cannot be
    // caught at run time without silently changing behaviour — a bad
    // `concurrency` would quietly stay sequential and a bad `on_item_error`
    // would quietly pick a default. Reject them here, where the message can name
    // the node.
    for node in &graph.nodes {
        let fans_out = matches!(
            node.kind,
            NodeKind::Agent
                | NodeKind::ToolCall
                | NodeKind::HttpRequest
                | NodeKind::Memory
                | NodeKind::SubWorkflow
        );

        if let Some(execution) = node.config.get("execution") {
            match execution.as_str() {
                Some("once" | "per_item") if fans_out => {}
                Some("once" | "per_item") => {
                    errors.push(ValidationError::InvalidNodeConfig {
                        node: node.id.clone(),
                        reason: format!(
                            "`execution` is not supported on a {} node (only agent, tool_call, \
                             http_request, memory, and sub_workflow map over their input)",
                            kind_name(&node.kind)
                        ),
                    });
                }
                _ => {
                    errors.push(ValidationError::InvalidNodeConfig {
                        node: node.id.clone(),
                        reason: format!(
                            "unknown `execution` value {execution} (expected \"once\" or \
                             \"per_item\")"
                        ),
                    });
                }
            }
        }

        // Whether this node actually maps over its input, accounting for the
        // per-kind default: `tool_call` / `http_request` / `memory` are per-item
        // unless told otherwise; `agent` / `sub_workflow` are not.
        let per_item = match node.config.get("execution").and_then(Value::as_str) {
            Some("per_item") => true,
            Some("once") => false,
            _ => matches!(
                node.kind,
                NodeKind::ToolCall | NodeKind::HttpRequest | NodeKind::Memory
            ),
        };

        for key in ["concurrency", "on_item_error"] {
            let Some(value) = node.config.get(key) else {
                continue;
            };
            // A fan-out knob on a node that runs once is a no-op, and a silent
            // no-op reads as "I asked for parallelism and got none".
            if !per_item {
                errors.push(ValidationError::InvalidNodeConfig {
                    node: node.id.clone(),
                    reason: format!(
                        "`{key}` has no effect without `execution: \"per_item\"` on a {} node",
                        kind_name(&node.kind)
                    ),
                });
                continue;
            }
            let ok = match key {
                "concurrency" => {
                    matches!(
                        value,
                        Value::Number(n) if n.as_u64().is_some(),
                    ) || value.as_str() == Some("all")
                }
                _ => matches!(value.as_str(), Some("collect" | "fail_fast" | "skip")),
            };
            if !ok {
                let expected = if key == "concurrency" {
                    "a non-negative integer or \"all\""
                } else {
                    "\"collect\", \"fail_fast\", or \"skip\""
                };
                errors.push(ValidationError::InvalidNodeConfig {
                    node: node.id.clone(),
                    reason: format!("`{key}` must be {expected}, got {value}"),
                });
            }
        }
    }

    // `memory` node config checks, including THE hard security invariant: a
    // `remember`/`forget` operation may never target `scope: "user"` — the
    // caller's durable, cross-flow memory. Rejecting this structurally, at the
    // door, means a workflow (or an LLM authoring one) can never plant or erase
    // durable facts about the user by way of a `remember`/`forget` node; the
    // only scope those two operations may write through is `"flow"`.
    for node in &graph.nodes {
        if node.kind != NodeKind::Memory {
            continue;
        }

        let operation = node.config.get("operation").and_then(Value::as_str);
        let Some(operation) = operation else {
            errors.push(ValidationError::InvalidNodeConfig {
                node: node.id.clone(),
                reason: "memory node requires `operation` (recall|search|flavour|people|\
                         remember|forget)"
                    .to_string(),
            });
            continue;
        };
        if !matches!(
            operation,
            "recall" | "search" | "flavour" | "people" | "remember" | "forget"
        ) {
            errors.push(ValidationError::InvalidNodeConfig {
                node: node.id.clone(),
                reason: format!(
                    "memory node has unknown operation {operation:?} (expected one of \
                     recall|search|flavour|people|remember|forget)"
                ),
            });
            continue;
        }

        let scope = node.config.get("scope").and_then(Value::as_str);

        // THE hard invariant (see the block comment above): reject before any
        // other config check, so it can never be masked by a different error.
        // remember/forget may write ONLY scope "flow". BOTH read-only scopes are
        // rejected here — "user" (the user's durable memory) and "flows"
        // (cross-flow read). This gate is unbypassable precisely because `scope`
        // is validated as a literal enum (below): an "=expr" binding resolves at
        // runtime and is never one of user|flow|flows, so it fails the enum
        // check and can never smuggle a write past this into
        // provider.remember/forget. If a future change makes `scope` bindable,
        // this invariant reopens — keep the enum check.
        if matches!(operation, "remember" | "forget")
            && matches!(scope, Some("user") | Some("flows"))
        {
            let bad = scope.unwrap_or_default();
            errors.push(ValidationError::InvalidNodeConfig {
                node: node.id.clone(),
                reason: format!(
                    "memory node operation {operation:?} may not target scope {bad:?} — \
                     remember/forget may only write scope \"flow\"; scopes \"user\" and \
                     \"flows\" are read-only from a workflow"
                ),
            });
        }

        if let Some(scope) = scope {
            if !matches!(scope, "user" | "flow" | "flows") {
                errors.push(ValidationError::InvalidNodeConfig {
                    node: node.id.clone(),
                    reason: format!(
                        "memory node has unknown scope {scope:?} (expected \
                         user|flow|flows)"
                    ),
                });
            }
        }

        // `scope` is required for recall/remember/forget (not search/flavour/
        // people — see the catalog contract for the exact per-operation table).
        if matches!(operation, "recall" | "remember" | "forget") && scope.is_none() {
            errors.push(ValidationError::InvalidNodeConfig {
                node: node.id.clone(),
                reason: format!("memory node operation {operation:?} requires `scope`"),
            });
        }

        if matches!(operation, "recall" | "search") {
            let has_query = node
                .config
                .get("query")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty());
            if !has_query {
                errors.push(ValidationError::InvalidNodeConfig {
                    node: node.id.clone(),
                    reason: format!("memory node operation {operation:?} requires `query`"),
                });
            }
        }

        if operation == "flavour" {
            let has_flavour = node
                .config
                .get("flavour")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty());
            if !has_flavour {
                errors.push(ValidationError::InvalidNodeConfig {
                    node: node.id.clone(),
                    reason: "memory node operation \"flavour\" requires `flavour` (slug)"
                        .to_string(),
                });
            }
        }

        if matches!(operation, "remember" | "forget") {
            let has_key = node
                .config
                .get("key")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty());
            if !has_key {
                errors.push(ValidationError::InvalidNodeConfig {
                    node: node.id.clone(),
                    reason: format!("memory node operation {operation:?} requires `key`"),
                });
            }
        }

        if operation == "remember" && node.config.get("value").is_none() {
            errors.push(ValidationError::InvalidNodeConfig {
                node: node.id.clone(),
                reason: "memory node operation \"remember\" requires `value`".to_string(),
            });
        }
    }

    // `dedup` node config checks: `key` (the per-item "=expr" dedup key) is the
    // only config field, and it is required — a dedup node with no `key` can
    // never resolve anything to compare, which is always an authoring mistake
    // (as opposed to a `key` that *resolves* to null at run time, which is the
    // intentional, per-item fail-open path the executor handles).
    for node in &graph.nodes {
        if node.kind != NodeKind::Dedup {
            continue;
        }
        let has_key = node
            .config
            .get("key")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty());
        if !has_key {
            errors.push(ValidationError::InvalidNodeConfig {
                node: node.id.clone(),
                reason: "dedup node requires `key` (an \"=expr\" resolved per item, e.g. \
                         \"=item.id\")"
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

    errors
}

/// Loop and cycle legality.
///
/// **Cycles are legal.** The engine lowers a back-edge as a plain re-entry and
/// the executor underneath is a super-step scheduler, so a graph that loops is
/// a supported graph, not a malformed one. What this pass refuses is the
/// narrow set of cycles that *cannot work* — each with a message naming the
/// fix, because "your loop silently ran once and stopped" is the failure mode
/// this replaces.
fn validate_loops(graph: &WorkflowGraph, errors: &mut Vec<ValidationError>) {
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

    // Every node that sits on some cycle, and the loop heads (back-edge
    // targets) those cycles close on.
    let heads: HashSet<&str> = loop_edges.iter().map(|(_, to)| to.as_str()).collect();
    let on_a_cycle: HashSet<&str> = loop_edges
        .iter()
        .flat_map(|(from, to)| nodes_on_cycle(graph, to, from))
        .collect();

    // A real fan-in `merge` inside the loop body deadlocks it. A single-input
    // merge is a passthrough and is not lowered as a waiting barrier.
    for id in &on_a_cycle {
        let is_merge = graph
            .nodes
            .iter()
            .any(|n| n.id == *id && n.kind == NodeKind::Merge);
        let forward_predecessors = graph
            .edges
            .iter()
            .filter(|edge| {
                edge.to_node == **id
                    && !loop_edges.contains(&(edge.from_node.clone(), edge.to_node.clone()))
            })
            .count();
        if is_merge && forward_predecessors > 1 {
            errors.push(ValidationError::IllegalCycle((*id).to_string()));
        }
    }

    for head in &heads {
        // A loop head that is also a fan-in cannot iterate: its forward
        // predecessors are lowered as waiting edges, and that barrier is
        // per-node, so it swallows the re-entry the back-edge delivers. The fix
        // is to join *before* the head — a `merge` outside the cycle — which
        // leaves the head with a single forward predecessor.
        let forward_predecessors = graph
            .edges
            .iter()
            .filter(|e| {
                e.to_node == **head
                    && !loop_edges.contains(&(e.from_node.clone(), e.to_node.clone()))
            })
            .count();
        if forward_predecessors > 1 {
            errors.push(ValidationError::IllegalCycle((*head).to_string()));
        }
    }

    // An unbounded cycle. Without a `loop` node to count passes, the only thing
    // standing between this graph and a run that spins until the host's wall
    // clock kills it is the trigger's `recursion_limit`. Requiring one of the
    // two makes the bound an authoring decision rather than an accident.
    let has_recursion_limit = graph
        .trigger()
        .and_then(|t| t.config.get("recursion_limit"))
        .and_then(Value::as_u64)
        .is_some_and(|n| n > 0);
    if !has_recursion_limit {
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

fn path_exists(graph: &WorkflowGraph, start: &str, target: &str) -> bool {
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
fn nodes_on_cycle<'g>(graph: &'g WorkflowGraph, start: &str, end: &str) -> HashSet<&'g str> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Edge, Node};

    fn node(id: &str, kind: NodeKind) -> Node {
        Node {
            id: id.to_string(),
            kind,
            type_version: 1,
            name: id.to_string(),
            config: serde_json::Value::Null,
            ports: Vec::new(),
            position: None,
        }
    }

    /// A graph with one trigger and no edges — the minimum that passes every
    /// structural check, so an inputs test sees only inputs errors.
    fn graph_with_inputs(inputs: Vec<crate::model::WorkflowInput>) -> WorkflowGraph {
        WorkflowGraph {
            inputs,
            nodes: vec![node("t", NodeKind::Trigger)],
            ..Default::default()
        }
    }

    #[test]
    fn accepts_declared_inputs() {
        use crate::model::{InputType, WorkflowInput};

        let graph = graph_with_inputs(vec![
            WorkflowInput::new("repo", InputType::String).required(),
            WorkflowInput::new("depth", InputType::Number).with_default(serde_json::json!(3)),
            WorkflowInput::new("payload", InputType::Json),
        ]);
        assert_eq!(validate(&graph), Ok(()));
    }

    #[test]
    fn rejects_duplicate_input_names() {
        use crate::model::{InputType, WorkflowInput};

        let graph = graph_with_inputs(vec![
            WorkflowInput::new("repo", InputType::String),
            WorkflowInput::new("repo", InputType::Number),
        ]);
        assert_eq!(
            validate(&graph),
            Err(ValidationError::DuplicateInputName("repo".to_string()))
        );
    }

    #[test]
    fn rejects_input_names_expressions_could_not_address() {
        use crate::model::{InputType, WorkflowInput};

        for bad in ["repo-url", "2fa", "", "repo.url"] {
            let graph = graph_with_inputs(vec![WorkflowInput::new(bad, InputType::String)]);
            assert_eq!(
                validate(&graph),
                Err(ValidationError::InvalidInputName(bad.to_string())),
                "{bad} should be rejected"
            );
        }
    }

    #[test]
    fn rejects_default_that_violates_its_own_type() {
        use crate::model::{InputType, WorkflowInput};

        let graph = graph_with_inputs(vec![
            WorkflowInput::new("depth", InputType::Number).with_default(serde_json::json!("3")),
        ]);
        assert_eq!(
            validate(&graph),
            Err(ValidationError::InputDefaultTypeMismatch {
                name: "depth".to_string(),
                expected: "number",
            })
        );
    }

    #[test]
    fn rejects_required_input_with_a_default() {
        use crate::model::{InputType, WorkflowInput};

        let graph = graph_with_inputs(vec![
            WorkflowInput::new("repo", InputType::String)
                .required()
                .with_default(serde_json::json!("acme/api")),
        ]);
        assert_eq!(
            validate(&graph),
            Err(ValidationError::RequiredInputWithDefault(
                "repo".to_string()
            ))
        );
    }

    #[test]
    fn collects_every_input_error_in_one_pass() {
        use crate::model::{InputType, WorkflowInput};

        let graph = graph_with_inputs(vec![
            WorkflowInput::new("repo-url", InputType::String),
            WorkflowInput::new("depth", InputType::Number).with_default(serde_json::json!("3")),
            WorkflowInput::new("depth", InputType::Number),
        ]);
        let errors = validate_all(&graph);
        assert_eq!(errors.len(), 3, "got {errors:?}");
        assert!(errors.contains(&ValidationError::InvalidInputName("repo-url".to_string())));
        assert!(errors.contains(&ValidationError::InputDefaultTypeMismatch {
            name: "depth".to_string(),
            expected: "number",
        }));
        assert!(errors.contains(&ValidationError::DuplicateInputName("depth".to_string())));
    }

    #[test]
    fn accepts_a_minimal_valid_graph() {
        let graph = WorkflowGraph {
            nodes: vec![node("t", NodeKind::Trigger), node("a", NodeKind::Agent)],
            edges: vec![Edge {
                from_node: "t".to_string(),
                from_port: "main".to_string(),
                to_node: "a".to_string(),
                to_port: "main".to_string(),
            }],
            ..Default::default()
        };
        assert_eq!(validate(&graph), Ok(()));
    }

    #[test]
    fn rejects_missing_trigger() {
        let graph = WorkflowGraph {
            nodes: vec![node("a", NodeKind::Agent)],
            ..Default::default()
        };
        assert_eq!(validate(&graph), Err(ValidationError::MissingTrigger));
    }

    #[test]
    fn rejects_multiple_triggers() {
        let graph = WorkflowGraph {
            nodes: vec![node("t1", NodeKind::Trigger), node("t2", NodeKind::Trigger)],
            ..Default::default()
        };
        assert!(matches!(
            validate(&graph),
            Err(ValidationError::MultipleTriggers(_))
        ));
    }

    #[test]
    fn rejects_duplicate_ids() {
        let graph = WorkflowGraph {
            nodes: vec![node("t", NodeKind::Trigger), node("t", NodeKind::Agent)],
            ..Default::default()
        };
        assert_eq!(
            validate(&graph),
            Err(ValidationError::DuplicateNodeId("t".to_string()))
        );
    }

    #[test]
    fn rejects_dangling_edge() {
        let graph = WorkflowGraph {
            nodes: vec![node("t", NodeKind::Trigger)],
            edges: vec![Edge {
                from_node: "t".to_string(),
                from_port: "main".to_string(),
                to_node: "ghost".to_string(),
                to_port: "main".to_string(),
            }],
            ..Default::default()
        };
        assert_eq!(
            validate(&graph),
            Err(ValidationError::UnknownNode("ghost".to_string()))
        );
    }

    #[test]
    fn rejects_empty_graph_as_missing_trigger() {
        let graph = WorkflowGraph::default();
        assert_eq!(validate(&graph), Err(ValidationError::MissingTrigger));
    }

    #[test]
    fn rejects_edge_with_unknown_from_node() {
        let graph = WorkflowGraph {
            nodes: vec![node("t", NodeKind::Trigger)],
            edges: vec![Edge {
                from_node: "ghost".to_string(),
                from_port: "main".to_string(),
                to_node: "t".to_string(),
                to_port: "main".to_string(),
            }],
            ..Default::default()
        };
        assert_eq!(
            validate(&graph),
            Err(ValidationError::UnknownNode("ghost".to_string()))
        );
    }

    #[test]
    fn rejects_edge_with_unknown_to_node() {
        let graph = WorkflowGraph {
            nodes: vec![node("t", NodeKind::Trigger), node("a", NodeKind::Agent)],
            edges: vec![Edge {
                from_node: "a".to_string(),
                from_port: "main".to_string(),
                to_node: "ghost".to_string(),
                to_port: "main".to_string(),
            }],
            ..Default::default()
        };
        assert_eq!(
            validate(&graph),
            Err(ValidationError::UnknownNode("ghost".to_string()))
        );
    }

    #[test]
    fn multiple_triggers_error_carries_all_ids() {
        let graph = WorkflowGraph {
            nodes: vec![
                node("t1", NodeKind::Trigger),
                node("t2", NodeKind::Trigger),
                node("t3", NodeKind::Trigger),
            ],
            ..Default::default()
        };
        match validate(&graph) {
            Err(ValidationError::MultipleTriggers(ids)) => {
                assert_eq!(ids, vec!["t1", "t2", "t3"]);
            }
            other => panic!("expected MultipleTriggers, got {other:?}"),
        }
    }

    fn sub_workflow_node(config: serde_json::Value) -> Node {
        let mut n = node("sw", NodeKind::SubWorkflow);
        n.config = config;
        n
    }

    fn graph_with_sub_workflow(config: serde_json::Value) -> WorkflowGraph {
        WorkflowGraph {
            nodes: vec![node("t", NodeKind::Trigger), sub_workflow_node(config)],
            ..Default::default()
        }
    }

    #[test]
    fn sub_workflow_accepts_inline_workflow() {
        let graph = graph_with_sub_workflow(serde_json::json!({
            "workflow": { "nodes": [], "edges": [] }
        }));
        assert_eq!(validate(&graph), Ok(()));
    }

    #[test]
    fn sub_workflow_accepts_workflow_id() {
        let graph = graph_with_sub_workflow(serde_json::json!({ "workflow_id": "child-1" }));
        assert_eq!(validate(&graph), Ok(()));
    }

    #[test]
    fn sub_workflow_rejects_both_inline_and_id() {
        let graph = graph_with_sub_workflow(serde_json::json!({
            "workflow": { "nodes": [], "edges": [] },
            "workflow_id": "child-1"
        }));
        assert!(matches!(
            validate(&graph),
            Err(ValidationError::InvalidNodeConfig { .. })
        ));
    }

    #[test]
    fn sub_workflow_rejects_neither_inline_nor_id() {
        // A blank `workflow_id` counts as absent.
        let graph = graph_with_sub_workflow(serde_json::json!({ "workflow_id": "" }));
        assert!(matches!(
            validate(&graph),
            Err(ValidationError::InvalidNodeConfig { .. })
        ));
        let graph = graph_with_sub_workflow(serde_json::Value::Null);
        assert!(matches!(
            validate(&graph),
            Err(ValidationError::InvalidNodeConfig { .. })
        ));
    }

    fn memory_node(id: &str, config: serde_json::Value) -> Node {
        let mut n = node(id, NodeKind::Memory);
        n.config = config;
        n
    }

    fn graph_with_memory_node(config: serde_json::Value) -> WorkflowGraph {
        WorkflowGraph {
            nodes: vec![node("t", NodeKind::Trigger), memory_node("mem", config)],
            edges: vec![Edge {
                from_node: "t".to_string(),
                from_port: "main".to_string(),
                to_node: "mem".to_string(),
                to_port: "main".to_string(),
            }],
            ..Default::default()
        }
    }

    // --- the hard invariant: remember/forget may never target scope "user" ---

    #[test]
    fn memory_rejects_remember_user_scope() {
        let graph = graph_with_memory_node(serde_json::json!({
            "operation": "remember", "scope": "user", "key": "k", "value": 1
        }));
        let err = validate(&graph).expect_err("remember·user must be rejected");
        match err {
            ValidationError::InvalidNodeConfig { node, reason } => {
                assert_eq!(node, "mem");
                assert!(reason.contains("\"user\""), "reason: {reason}");
                assert!(reason.contains("remember"), "reason: {reason}");
            }
            other => panic!("expected InvalidNodeConfig, got {other:?}"),
        }
    }

    #[test]
    fn memory_rejects_forget_user_scope() {
        let graph = graph_with_memory_node(serde_json::json!({
            "operation": "forget", "scope": "user", "key": "k"
        }));
        let err = validate(&graph).expect_err("forget·user must be rejected");
        match err {
            ValidationError::InvalidNodeConfig { node, reason } => {
                assert_eq!(node, "mem");
                assert!(reason.contains("\"user\""), "reason: {reason}");
                assert!(reason.contains("forget"), "reason: {reason}");
            }
            other => panic!("expected InvalidNodeConfig, got {other:?}"),
        }
    }

    #[test]
    fn memory_rejects_remember_flows_scope() {
        // "flows" is a read-only cross-flow scope — a write to it must be
        // rejected at validate time, not just backstopped by the host adapter.
        let graph = graph_with_memory_node(serde_json::json!({
            "operation": "remember", "scope": "flows", "key": "k", "value": 1
        }));
        let err = validate(&graph).expect_err("remember·flows must be rejected");
        match err {
            ValidationError::InvalidNodeConfig { node, reason } => {
                assert_eq!(node, "mem");
                assert!(reason.contains("\"flows\""), "reason: {reason}");
                assert!(reason.contains("remember"), "reason: {reason}");
            }
            other => panic!("expected InvalidNodeConfig, got {other:?}"),
        }
    }

    #[test]
    fn memory_rejects_forget_flows_scope() {
        let graph = graph_with_memory_node(serde_json::json!({
            "operation": "forget", "scope": "flows", "key": "k"
        }));
        let err = validate(&graph).expect_err("forget·flows must be rejected");
        match err {
            ValidationError::InvalidNodeConfig { node, reason } => {
                assert_eq!(node, "mem");
                assert!(reason.contains("\"flows\""), "reason: {reason}");
                assert!(reason.contains("forget"), "reason: {reason}");
            }
            other => panic!("expected InvalidNodeConfig, got {other:?}"),
        }
    }

    #[test]
    fn memory_accepts_remember_flow_scope() {
        let graph = graph_with_memory_node(serde_json::json!({
            "operation": "remember", "scope": "flow", "key": "k", "value": { "v": 1 }
        }));
        assert_eq!(validate(&graph), Ok(()));
    }

    #[test]
    fn memory_accepts_forget_flow_scope() {
        let graph = graph_with_memory_node(serde_json::json!({
            "operation": "forget", "scope": "flow", "key": "k"
        }));
        assert_eq!(validate(&graph), Ok(()));
    }

    #[test]
    fn memory_rejects_unknown_scope_value() {
        let graph = graph_with_memory_node(serde_json::json!({
            "operation": "recall", "scope": "everyone", "query": "x"
        }));
        assert!(matches!(
            validate(&graph),
            Err(ValidationError::InvalidNodeConfig { .. })
        ));
    }

    // --- required-field checks per operation ---

    #[test]
    fn memory_recall_accepts_user_and_flows_scope() {
        // Only remember/forget are scope-restricted; reads may target any
        // declared scope, including the read-only ones.
        for scope in ["user", "flow", "flows"] {
            let graph = graph_with_memory_node(serde_json::json!({
                "operation": "recall", "scope": scope, "query": "x"
            }));
            assert_eq!(
                validate(&graph),
                Ok(()),
                "scope {scope} should be valid for recall"
            );
        }
    }

    #[test]
    fn memory_requires_operation() {
        let graph = graph_with_memory_node(serde_json::json!({ "scope": "flow" }));
        match validate(&graph) {
            Err(ValidationError::InvalidNodeConfig { node, reason }) => {
                assert_eq!(node, "mem");
                assert!(reason.contains("operation"), "reason: {reason}");
            }
            other => panic!("expected InvalidNodeConfig, got {other:?}"),
        }
    }

    #[test]
    fn memory_rejects_unknown_operation() {
        let graph = graph_with_memory_node(serde_json::json!({ "operation": "levitate" }));
        assert!(matches!(
            validate(&graph),
            Err(ValidationError::InvalidNodeConfig { .. })
        ));
    }

    #[test]
    fn memory_recall_requires_scope() {
        let graph = graph_with_memory_node(serde_json::json!({
            "operation": "recall", "query": "x"
        }));
        match validate(&graph) {
            Err(ValidationError::InvalidNodeConfig { node, reason }) => {
                assert_eq!(node, "mem");
                assert!(reason.contains("scope"), "reason: {reason}");
            }
            other => panic!("expected InvalidNodeConfig, got {other:?}"),
        }
    }

    #[test]
    fn memory_recall_requires_query() {
        let graph = graph_with_memory_node(serde_json::json!({
            "operation": "recall", "scope": "flow"
        }));
        match validate(&graph) {
            Err(ValidationError::InvalidNodeConfig { node, reason }) => {
                assert_eq!(node, "mem");
                assert!(reason.contains("query"), "reason: {reason}");
            }
            other => panic!("expected InvalidNodeConfig, got {other:?}"),
        }
    }

    #[test]
    fn memory_search_requires_query_but_not_scope() {
        let missing_query = graph_with_memory_node(serde_json::json!({ "operation": "search" }));
        assert!(matches!(
            validate(&missing_query),
            Err(ValidationError::InvalidNodeConfig { .. })
        ));

        let no_scope_ok = graph_with_memory_node(serde_json::json!({
            "operation": "search", "query": "x"
        }));
        assert_eq!(validate(&no_scope_ok), Ok(()));
    }

    #[test]
    fn memory_flavour_requires_flavour_slug() {
        let graph = graph_with_memory_node(serde_json::json!({ "operation": "flavour" }));
        match validate(&graph) {
            Err(ValidationError::InvalidNodeConfig { node, reason }) => {
                assert_eq!(node, "mem");
                assert!(reason.contains("flavour"), "reason: {reason}");
            }
            other => panic!("expected InvalidNodeConfig, got {other:?}"),
        }
        let ok = graph_with_memory_node(serde_json::json!({
            "operation": "flavour", "flavour": "email-tone"
        }));
        assert_eq!(validate(&ok), Ok(()));
    }

    #[test]
    fn memory_people_requires_nothing() {
        // `people` has no required `scope`/`query` — an empty config is valid.
        let graph = graph_with_memory_node(serde_json::json!({ "operation": "people" }));
        assert_eq!(validate(&graph), Ok(()));
    }

    #[test]
    fn memory_remember_requires_key_and_value() {
        let missing_both = graph_with_memory_node(serde_json::json!({
            "operation": "remember", "scope": "flow"
        }));
        match validate(&missing_both) {
            Err(ValidationError::InvalidNodeConfig { node, reason }) => {
                assert_eq!(node, "mem");
                assert!(reason.contains("key"), "reason: {reason}");
            }
            other => panic!("expected InvalidNodeConfig (key), got {other:?}"),
        }

        let missing_value = graph_with_memory_node(serde_json::json!({
            "operation": "remember", "scope": "flow", "key": "k"
        }));
        match validate(&missing_value) {
            Err(ValidationError::InvalidNodeConfig { node, reason }) => {
                assert_eq!(node, "mem");
                assert!(reason.contains("value"), "reason: {reason}");
            }
            other => panic!("expected InvalidNodeConfig (value), got {other:?}"),
        }
    }

    #[test]
    fn memory_forget_requires_key_but_not_value() {
        let missing_key = graph_with_memory_node(serde_json::json!({
            "operation": "forget", "scope": "flow"
        }));
        assert!(matches!(
            validate(&missing_key),
            Err(ValidationError::InvalidNodeConfig { .. })
        ));

        let ok = graph_with_memory_node(serde_json::json!({
            "operation": "forget", "scope": "flow", "key": "k"
        }));
        assert_eq!(validate(&ok), Ok(()));
    }

    fn dedup_node(id: &str, config: serde_json::Value) -> Node {
        let mut n = node(id, NodeKind::Dedup);
        n.config = config;
        n
    }

    fn graph_with_dedup_node(config: serde_json::Value) -> WorkflowGraph {
        WorkflowGraph {
            nodes: vec![node("t", NodeKind::Trigger), dedup_node("dd", config)],
            edges: vec![Edge {
                from_node: "t".to_string(),
                from_port: "main".to_string(),
                to_node: "dd".to_string(),
                to_port: "main".to_string(),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn dedup_accepts_a_key_expression() {
        let graph = graph_with_dedup_node(serde_json::json!({ "key": "=item.id" }));
        assert_eq!(validate(&graph), Ok(()));
    }

    #[test]
    fn dedup_rejects_missing_key() {
        let graph = graph_with_dedup_node(serde_json::Value::Null);
        match validate(&graph) {
            Err(ValidationError::InvalidNodeConfig { node, reason }) => {
                assert_eq!(node, "dd");
                assert!(reason.contains("key"), "reason: {reason}");
            }
            other => panic!("expected InvalidNodeConfig, got {other:?}"),
        }
    }

    #[test]
    fn dedup_rejects_empty_key() {
        let graph = graph_with_dedup_node(serde_json::json!({ "key": "" }));
        assert!(matches!(
            validate(&graph),
            Err(ValidationError::InvalidNodeConfig { .. })
        ));
    }

    #[test]
    fn dedup_rejects_non_string_key() {
        // `key` is a literal "=expr" string in config — a non-string value
        // (e.g. authored as a bare number) is just as much a missing key.
        let graph = graph_with_dedup_node(serde_json::json!({ "key": 1 }));
        assert!(matches!(
            validate(&graph),
            Err(ValidationError::InvalidNodeConfig { .. })
        ));
    }

    fn tool_node(id: &str, config: serde_json::Value) -> Node {
        let mut n = node(id, NodeKind::ToolCall);
        n.config = config;
        n
    }

    #[test]
    fn rejects_on_error_route_without_error_edge() {
        // A `route` policy with no outgoing `error` edge would drop the routed
        // error item silently — reject it.
        let graph = WorkflowGraph {
            nodes: vec![
                node("t", NodeKind::Trigger),
                tool_node("x", serde_json::json!({ "on_error": "route" })),
            ],
            edges: vec![Edge {
                from_node: "t".to_string(),
                from_port: "main".to_string(),
                to_node: "x".to_string(),
                to_port: "main".to_string(),
            }],
            ..Default::default()
        };
        assert_eq!(
            validate(&graph),
            Err(ValidationError::MissingErrorRoute("x".to_string()))
        );
    }

    #[test]
    fn accepts_on_error_route_with_error_edge() {
        // The same graph is valid once an edge leaves the node's `error` port.
        let graph = WorkflowGraph {
            nodes: vec![
                node("t", NodeKind::Trigger),
                tool_node("x", serde_json::json!({ "on_error": "route" })),
                node("recover", NodeKind::Agent),
            ],
            edges: vec![
                Edge {
                    from_node: "t".to_string(),
                    from_port: "main".to_string(),
                    to_node: "x".to_string(),
                    to_port: "main".to_string(),
                },
                Edge {
                    from_node: "x".to_string(),
                    from_port: "error".to_string(),
                    to_node: "recover".to_string(),
                    to_port: "main".to_string(),
                },
            ],
            ..Default::default()
        };
        assert_eq!(validate(&graph), Ok(()));
    }

    #[test]
    fn accepts_on_error_stop_and_continue_without_error_edge() {
        for policy in ["stop", "continue"] {
            let graph = WorkflowGraph {
                nodes: vec![
                    node("t", NodeKind::Trigger),
                    tool_node("x", serde_json::json!({ "on_error": policy })),
                ],
                edges: vec![Edge {
                    from_node: "t".to_string(),
                    from_port: "main".to_string(),
                    to_node: "x".to_string(),
                    to_port: "main".to_string(),
                }],
                ..Default::default()
            };
            assert_eq!(validate(&graph), Ok(()), "policy {policy} should be valid");
        }
    }

    #[test]
    fn rejects_unknown_on_error_value() {
        let graph = WorkflowGraph {
            nodes: vec![
                node("t", NodeKind::Trigger),
                tool_node("x", serde_json::json!({ "on_error": "explode" })),
            ],
            ..Default::default()
        };
        assert_eq!(
            validate(&graph),
            Err(ValidationError::InvalidOnError {
                node: "x".to_string(),
                value: "explode".to_string(),
            })
        );
    }

    #[test]
    fn rejects_duplicate_edges() {
        let dup = || Edge {
            from_node: "t".to_string(),
            from_port: "main".to_string(),
            to_node: "a".to_string(),
            to_port: "main".to_string(),
        };
        let graph = WorkflowGraph {
            nodes: vec![node("t", NodeKind::Trigger), node("a", NodeKind::Agent)],
            edges: vec![dup(), dup()],
            ..Default::default()
        };
        assert_eq!(
            validate(&graph),
            Err(ValidationError::DuplicateEdge {
                from_node: "t".to_string(),
                from_port: "main".to_string(),
                to_node: "a".to_string(),
                to_port: "main".to_string(),
            })
        );
    }

    #[test]
    fn accepts_parallel_edges_on_distinct_ports() {
        // Two edges between the same node pair are fine as long as they differ
        // in port — only fully identical edges are rejected.
        let graph = WorkflowGraph {
            nodes: vec![node("t", NodeKind::Trigger), node("a", NodeKind::Agent)],
            edges: vec![
                Edge {
                    from_node: "t".to_string(),
                    from_port: "main".to_string(),
                    to_node: "a".to_string(),
                    to_port: "main".to_string(),
                },
                Edge {
                    from_node: "t".to_string(),
                    from_port: "main".to_string(),
                    to_node: "a".to_string(),
                    to_port: "other".to_string(),
                },
            ],
            ..Default::default()
        };
        assert_eq!(validate(&graph), Ok(()));
    }

    fn condition_node(id: &str) -> Node {
        node(id, NodeKind::Condition)
    }

    #[test]
    fn accepts_condition_with_branch_label_on_from_port() {
        // The CORRECT shape (B23/B24): the branch label lives on `from_port`,
        // `to_port` stays `"main"`.
        let graph = WorkflowGraph {
            nodes: vec![
                node("t", NodeKind::Trigger),
                condition_node("gate"),
                node("yes", NodeKind::Agent),
                node("no", NodeKind::Agent),
            ],
            edges: vec![
                Edge {
                    from_node: "t".to_string(),
                    from_port: "main".to_string(),
                    to_node: "gate".to_string(),
                    to_port: "main".to_string(),
                },
                Edge {
                    from_node: "gate".to_string(),
                    from_port: "true".to_string(),
                    to_node: "yes".to_string(),
                    to_port: "main".to_string(),
                },
                Edge {
                    from_node: "gate".to_string(),
                    from_port: "false".to_string(),
                    to_node: "no".to_string(),
                    to_port: "main".to_string(),
                },
            ],
            ..Default::default()
        };
        assert_eq!(validate(&graph), Ok(()));
    }

    #[test]
    fn accepts_condition_with_only_one_branch_wired() {
        // Wiring only the `true` (or only the `false`) branch is legal — the
        // other simply dead-ends.
        let graph = WorkflowGraph {
            nodes: vec![
                node("t", NodeKind::Trigger),
                condition_node("gate"),
                node("yes", NodeKind::Agent),
            ],
            edges: vec![
                Edge {
                    from_node: "t".to_string(),
                    from_port: "main".to_string(),
                    to_node: "gate".to_string(),
                    to_port: "main".to_string(),
                },
                Edge {
                    from_node: "gate".to_string(),
                    from_port: "true".to_string(),
                    to_node: "yes".to_string(),
                    to_port: "main".to_string(),
                },
            ],
            ..Default::default()
        };
        assert_eq!(validate(&graph), Ok(()));
    }

    #[test]
    fn rejects_condition_with_branch_label_on_to_port_instead_of_from_port() {
        // The BAD shape (B23/B24 — the exact bug the workflow_builder agent
        // produced live): both edges share `from_port: "main"` with the branch
        // label on `to_port` instead. Without this check, `handler_routing`
        // would see one `from_port` group with two targets and classify it as
        // a parallel `FanOut`, silently driving BOTH branches unconditionally.
        let graph = WorkflowGraph {
            nodes: vec![
                node("t", NodeKind::Trigger),
                condition_node("gate"),
                node("yes", NodeKind::Agent),
                node("no", NodeKind::Agent),
            ],
            edges: vec![
                Edge {
                    from_node: "t".to_string(),
                    from_port: "main".to_string(),
                    to_node: "gate".to_string(),
                    to_port: "main".to_string(),
                },
                Edge {
                    from_node: "gate".to_string(),
                    from_port: "main".to_string(),
                    to_node: "yes".to_string(),
                    to_port: "true".to_string(),
                },
                Edge {
                    from_node: "gate".to_string(),
                    from_port: "main".to_string(),
                    to_node: "no".to_string(),
                    to_port: "false".to_string(),
                },
            ],
            ..Default::default()
        };
        assert_eq!(
            validate(&graph),
            Err(ValidationError::InvalidConditionRouting {
                node: "gate".to_string(),
                from_port: "main".to_string(),
            })
        );
    }

    #[test]
    fn rejects_condition_with_unrecognized_from_port() {
        let graph = WorkflowGraph {
            nodes: vec![
                node("t", NodeKind::Trigger),
                condition_node("gate"),
                node("other", NodeKind::Agent),
            ],
            edges: vec![
                Edge {
                    from_node: "t".to_string(),
                    from_port: "main".to_string(),
                    to_node: "gate".to_string(),
                    to_port: "main".to_string(),
                },
                Edge {
                    from_node: "gate".to_string(),
                    from_port: "maybe".to_string(),
                    to_node: "other".to_string(),
                    to_port: "main".to_string(),
                },
            ],
            ..Default::default()
        };
        assert_eq!(
            validate(&graph),
            Err(ValidationError::InvalidConditionRouting {
                node: "gate".to_string(),
                from_port: "maybe".to_string(),
            })
        );
    }

    #[test]
    fn duplicate_id_is_reported_before_trigger_checks() {
        // Two agents sharing an id and no trigger: the duplicate-id check runs
        // first, so that is the error surfaced.
        let graph = WorkflowGraph {
            nodes: vec![node("dup", NodeKind::Agent), node("dup", NodeKind::Agent)],
            ..Default::default()
        };
        assert_eq!(
            validate(&graph),
            Err(ValidationError::DuplicateNodeId("dup".to_string()))
        );
    }

    #[test]
    fn validate_all_is_empty_for_a_valid_graph() {
        let graph = WorkflowGraph {
            nodes: vec![node("t", NodeKind::Trigger), node("a", NodeKind::Agent)],
            edges: vec![Edge {
                from_node: "t".to_string(),
                from_port: "main".to_string(),
                to_node: "a".to_string(),
                to_port: "main".to_string(),
            }],
            ..Default::default()
        };
        assert!(validate_all(&graph).is_empty());
    }

    #[test]
    fn validate_all_first_element_matches_validate() {
        // The single-error contract of `validate` must stay exactly the first
        // element of `validate_all` — same graph, same lead error.
        let graph = WorkflowGraph {
            nodes: vec![node("dup", NodeKind::Agent), node("dup", NodeKind::Agent)],
            ..Default::default()
        };
        assert_eq!(
            validate_all(&graph).into_iter().next(),
            validate(&graph).err()
        );
    }

    #[test]
    fn validate_all_accumulates_independent_errors() {
        // A graph riddled with problems: no trigger, a duplicate node id, a
        // dangling edge, an unknown on_error value, and a mis-wired condition.
        // One pass should surface all of them, not just the first.
        let graph = WorkflowGraph {
            nodes: vec![
                node("dup", NodeKind::Agent),
                node("dup", NodeKind::Agent),
                condition_node("gate"),
                tool_node("x", serde_json::json!({ "on_error": "explode" })),
            ],
            edges: vec![Edge {
                from_node: "gate".to_string(),
                from_port: "maybe".to_string(),
                to_node: "ghost".to_string(),
                to_port: "main".to_string(),
            }],
            ..Default::default()
        };
        let errors = validate_all(&graph);
        assert!(
            errors.contains(&ValidationError::DuplicateNodeId("dup".to_string())),
            "missing duplicate-id error in {errors:?}"
        );
        assert!(
            errors.contains(&ValidationError::MissingTrigger),
            "missing trigger error in {errors:?}"
        );
        assert!(
            errors.contains(&ValidationError::UnknownNode("ghost".to_string())),
            "missing unknown-node error in {errors:?}"
        );
        assert!(
            errors.contains(&ValidationError::InvalidOnError {
                node: "x".to_string(),
                value: "explode".to_string(),
            }),
            "missing invalid-on_error error in {errors:?}"
        );
        assert!(
            errors.contains(&ValidationError::InvalidConditionRouting {
                node: "gate".to_string(),
                from_port: "maybe".to_string(),
            }),
            "missing condition-routing error in {errors:?}"
        );
        // Five distinct problems, five errors — no fail-fast truncation.
        assert!(
            errors.len() >= 5,
            "expected >=5 accumulated errors, got {errors:?}"
        );
    }

    #[test]
    fn validate_all_reports_every_duplicate_and_every_dangling_edge() {
        // Two separate dangling edges must both be reported (fail-fast would
        // stop at the first).
        let graph = WorkflowGraph {
            nodes: vec![node("t", NodeKind::Trigger)],
            edges: vec![
                Edge {
                    from_node: "t".to_string(),
                    from_port: "main".to_string(),
                    to_node: "ghost1".to_string(),
                    to_port: "main".to_string(),
                },
                Edge {
                    from_node: "t".to_string(),
                    from_port: "main".to_string(),
                    to_node: "ghost2".to_string(),
                    to_port: "main".to_string(),
                },
            ],
            ..Default::default()
        };
        let errors = validate_all(&graph);
        assert!(errors.contains(&ValidationError::UnknownNode("ghost1".to_string())));
        assert!(errors.contains(&ValidationError::UnknownNode("ghost2".to_string())));
    }

    #[test]
    fn validation_error_code_and_node_id_accessors() {
        assert_eq!(ValidationError::MissingTrigger.code(), "missing_trigger");
        assert_eq!(ValidationError::MissingTrigger.node_id(), None);
        assert_eq!(
            ValidationError::UnknownNode("ghost".to_string()).code(),
            "unknown_node"
        );
        assert_eq!(
            ValidationError::UnknownNode("ghost".to_string()).node_id(),
            Some("ghost")
        );
        assert_eq!(
            ValidationError::InvalidConditionRouting {
                node: "gate".to_string(),
                from_port: "main".to_string(),
            }
            .node_id(),
            Some("gate")
        );
        assert_eq!(
            ValidationError::MultipleTriggers(vec!["a".to_string()]).node_id(),
            None
        );
    }
}

#[cfg(test)]
mod fanout_tests {
    use super::{validate, validate_all};
    use crate::error::ValidationError;
    use crate::model::{Node, NodeKind, WorkflowGraph};
    use serde_json::{Value, json};

    /// A trigger plus one configured node of `kind` — the smallest graph that
    /// exercises a per-kind config check.
    fn graph(kind: NodeKind, config: Value) -> WorkflowGraph {
        let mk = |id: &str, kind: NodeKind, config: Value| Node {
            id: id.to_string(),
            kind,
            type_version: 1,
            name: id.to_string(),
            config,
            ports: Vec::new(),
            position: None,
        };
        WorkflowGraph {
            nodes: vec![
                mk("t", NodeKind::Trigger, Value::Null),
                mk("n", kind, config),
            ],
            ..Default::default()
        }
    }

    /// The `reason` of the single `InvalidNodeConfig` error, or a panic.
    fn reason(kind: NodeKind, config: Value) -> String {
        match validate_all(&graph(kind, config))
            .into_iter()
            .find(|e| matches!(e, ValidationError::InvalidNodeConfig { .. }))
        {
            Some(ValidationError::InvalidNodeConfig { reason, .. }) => reason,
            other => panic!("expected an InvalidNodeConfig error, got {other:?}"),
        }
    }

    #[test]
    fn a_valid_fan_out_passes() {
        assert_eq!(
            validate(&graph(
                NodeKind::Agent,
                json!({ "execution": "per_item", "concurrency": 8, "on_item_error": "collect" })
            )),
            Ok(())
        );
        // `"all"` and `0` are both legal spellings of unbounded.
        for c in [json!("all"), json!(0)] {
            assert_eq!(
                validate(&graph(
                    NodeKind::ToolCall,
                    json!({ "execution": "per_item", "concurrency": c })
                )),
                Ok(())
            );
        }
    }

    #[test]
    fn per_item_default_kinds_may_carry_fan_out_config_without_declaring_execution() {
        // tool_call / http_request / memory are per-item by default, so the
        // knobs apply without an explicit `execution`.
        for kind in [NodeKind::ToolCall, NodeKind::HttpRequest] {
            assert_eq!(
                validate(&graph(kind.clone(), json!({ "concurrency": 4 }))),
                Ok(()),
                "{kind:?} is per-item by default"
            );
        }
    }

    #[test]
    fn concurrency_on_a_once_node_is_rejected_rather_than_silently_ignored() {
        // `agent` defaults to `once`, so this author asked for parallelism and
        // would otherwise have got none, with no signal at all.
        let reason = reason(NodeKind::Agent, json!({ "concurrency": 8 }));
        assert!(
            reason.contains("no effect") && reason.contains("per_item"),
            "expected a no-effect explanation, got: {reason}"
        );

        // Explicitly opting out is the same story.
        let reason = reason_of(
            NodeKind::ToolCall,
            json!({ "execution": "once", "concurrency": 8 }),
        );
        assert!(reason.contains("no effect"), "got: {reason}");
    }

    fn reason_of(kind: NodeKind, config: Value) -> String {
        reason(kind, config)
    }

    #[test]
    fn a_malformed_concurrency_is_rejected() {
        for bad in [json!("lots"), json!(-1), json!(1.5), json!(true)] {
            let reason = reason(
                NodeKind::ToolCall,
                json!({ "execution": "per_item", "concurrency": bad }),
            );
            assert!(
                reason.contains("concurrency"),
                "expected a concurrency error for {bad}, got: {reason}"
            );
        }
    }

    #[test]
    fn an_unknown_item_error_policy_is_rejected() {
        let reason = reason(
            NodeKind::ToolCall,
            json!({ "execution": "per_item", "on_item_error": "explode" }),
        );
        assert!(
            reason.contains("on_item_error") && reason.contains("collect"),
            "expected the allowed policies to be listed, got: {reason}"
        );
    }

    #[test]
    fn an_unknown_execution_value_is_rejected() {
        let reason = reason(NodeKind::Agent, json!({ "execution": "parallel" }));
        assert!(
            reason.contains("execution") && reason.contains("per_item"),
            "expected the allowed modes to be listed, got: {reason}"
        );
    }

    #[test]
    fn execution_on_a_kind_that_cannot_map_is_rejected() {
        // A `transform` node does not map over its input; accepting `execution`
        // there would imply a fan-out that never happens.
        let reason = reason(NodeKind::Transform, json!({ "execution": "per_item" }));
        assert!(
            reason.contains("not supported") && reason.contains("transform"),
            "expected the kind to be named in its wire spelling, got: {reason}"
        );
    }
}

#[cfg(test)]
mod loop_tests {
    use super::*;
    use crate::model::{Edge, Node};

    /// Builds a node with no config — the shape most nodes in these graphs take.
    fn node(id: &str, kind: NodeKind) -> Node {
        node_cfg(id, kind, serde_json::Value::Null)
    }

    /// Builds a node with an explicit config, which a `loop` node needs.
    fn node_cfg(id: &str, kind: NodeKind, config: serde_json::Value) -> Node {
        Node {
            id: id.to_string(),
            kind,
            type_version: 1,
            name: id.to_string(),
            config,
            ports: Vec::new(),
            position: None,
        }
    }

    fn edge_on(from: &str, port: &str, to: &str) -> Edge {
        Edge {
            from_node: from.to_string(),
            from_port: port.to_string(),
            to_node: to.to_string(),
            to_port: "main".to_string(),
        }
    }

    /// The canonical bounded loop must pass clean — the point of the pass is to
    /// permit cycles, not to refuse them.
    #[test]
    fn accepts_a_bounded_loop() {
        let graph = WorkflowGraph {
            nodes: vec![
                node("t", NodeKind::Trigger),
                node_cfg(
                    "l",
                    NodeKind::Loop,
                    serde_json::json!({ "max_iterations": 3 }),
                ),
                node("work", NodeKind::OutputParser),
                node("out", NodeKind::OutputParser),
            ],
            edges: vec![
                edge_on("t", "main", "l"),
                edge_on("l", "body", "work"),
                edge_on("work", "main", "l"),
                edge_on("l", "done", "out"),
            ],
            ..Default::default()
        };
        assert_eq!(validate_all(&graph), Vec::new());
    }

    #[test]
    fn rejects_a_zero_max_iterations() {
        let graph = WorkflowGraph {
            nodes: vec![
                node("t", NodeKind::Trigger),
                node_cfg(
                    "l",
                    NodeKind::Loop,
                    serde_json::json!({ "max_iterations": 0 }),
                ),
                node("work", NodeKind::OutputParser),
            ],
            edges: vec![
                edge_on("t", "main", "l"),
                edge_on("l", "body", "work"),
                edge_on("work", "main", "l"),
            ],
            ..Default::default()
        };
        assert!(
            validate_all(&graph).iter().any(|e| matches!(
                e,
                ValidationError::InvalidNodeConfig { node, reason }
                    if node == "l" && reason.contains("positive")
            )),
            "a zero cap should be refused"
        );
    }

    #[test]
    fn rejects_an_unknown_on_exceeded_policy() {
        let graph = WorkflowGraph {
            nodes: vec![
                node("t", NodeKind::Trigger),
                node_cfg(
                    "l",
                    NodeKind::Loop,
                    serde_json::json!({ "max_iterations": 2, "on_exceeded": "shrug" }),
                ),
                node("work", NodeKind::OutputParser),
            ],
            edges: vec![
                edge_on("t", "main", "l"),
                edge_on("l", "body", "work"),
                edge_on("work", "main", "l"),
            ],
            ..Default::default()
        };
        assert!(
            validate_all(&graph).iter().any(|e| matches!(
                e,
                ValidationError::InvalidNodeConfig { node, reason }
                    if node == "l" && reason.contains("on_exceeded")
            )),
            "an unknown policy should be refused"
        );
    }

    /// A loop head with nothing wired to `body` can never iterate, which is
    /// almost always a half-finished graph rather than an intent.
    #[test]
    fn rejects_a_loop_with_no_body_edge() {
        let graph = WorkflowGraph {
            nodes: vec![
                node("t", NodeKind::Trigger),
                node_cfg(
                    "l",
                    NodeKind::Loop,
                    serde_json::json!({ "max_iterations": 2 }),
                ),
                node("out", NodeKind::OutputParser),
            ],
            edges: vec![edge_on("t", "main", "l"), edge_on("l", "done", "out")],
            ..Default::default()
        };
        assert!(
            validate_all(&graph).iter().any(|e| matches!(
                e,
                ValidationError::InvalidNodeConfig { node, reason }
                    if node == "l" && reason.contains("`body`")
            )),
            "a loop that cannot iterate should be refused"
        );
    }

    #[test]
    fn rejects_a_loop_body_that_never_returns_to_its_head() {
        let graph = WorkflowGraph {
            nodes: vec![
                node("t", NodeKind::Trigger),
                node_cfg(
                    "l",
                    NodeKind::Loop,
                    serde_json::json!({ "max_iterations": 2 }),
                ),
                node("work", NodeKind::OutputParser),
            ],
            edges: vec![edge_on("t", "main", "l"), edge_on("l", "body", "work")],
            ..Default::default()
        };

        assert!(validate_all(&graph).iter().any(|error| matches!(
            error,
            ValidationError::InvalidNodeConfig { node, reason }
                if node == "l" && reason.contains("route back")
        )));
    }

    #[test]
    fn accepts_a_single_input_merge_inside_a_loop() {
        let graph = WorkflowGraph {
            nodes: vec![
                node("t", NodeKind::Trigger),
                node_cfg(
                    "l",
                    NodeKind::Loop,
                    serde_json::json!({ "max_iterations": 2 }),
                ),
                node("m", NodeKind::Merge),
            ],
            edges: vec![
                edge_on("t", "main", "l"),
                edge_on("l", "body", "m"),
                edge_on("m", "main", "l"),
            ],
            ..Default::default()
        };

        assert_eq!(validate_all(&graph), Vec::new());
    }

    #[test]
    fn rejects_a_real_fan_in_merge_inside_a_loop() {
        let graph = WorkflowGraph {
            nodes: vec![
                node("t", NodeKind::Trigger),
                node("seed_a", NodeKind::OutputParser),
                node("seed_b", NodeKind::OutputParser),
                node_cfg(
                    "l",
                    NodeKind::Loop,
                    serde_json::json!({ "max_iterations": 2 }),
                ),
                node("work", NodeKind::OutputParser),
                node("m", NodeKind::Merge),
                node("tail", NodeKind::OutputParser),
            ],
            edges: vec![
                edge_on("t", "main", "seed_a"),
                edge_on("t", "main", "seed_b"),
                edge_on("seed_a", "main", "m"),
                edge_on("seed_b", "main", "m"),
                edge_on("l", "body", "work"),
                edge_on("work", "main", "m"),
                edge_on("m", "main", "tail"),
                edge_on("tail", "main", "l"),
            ],
            ..Default::default()
        };

        assert!(
            validate_all(&graph)
                .iter()
                .any(|error| matches!(error, ValidationError::IllegalCycle(node) if node == "m"))
        );
    }

    /// An acyclic graph must never reach the cycle branches — a regression here
    /// would refuse ordinary workflows.
    #[test]
    fn an_acyclic_graph_raises_no_loop_errors() {
        let graph = WorkflowGraph {
            nodes: vec![
                node("t", NodeKind::Trigger),
                node("a", NodeKind::OutputParser),
                node("m", NodeKind::Merge),
            ],
            edges: vec![edge_on("t", "main", "a"), edge_on("a", "main", "m")],
            ..Default::default()
        };
        assert_eq!(validate_all(&graph), Vec::new());
    }

    /// A `merge` off the cycle is fine — only one sitting *on* it deadlocks.
    #[test]
    fn a_merge_outside_the_cycle_is_accepted() {
        let graph = WorkflowGraph {
            nodes: vec![
                node("t", NodeKind::Trigger),
                node("left", NodeKind::OutputParser),
                node("right", NodeKind::OutputParser),
                node("m", NodeKind::Merge),
                node_cfg(
                    "l",
                    NodeKind::Loop,
                    serde_json::json!({ "max_iterations": 2 }),
                ),
                node("work", NodeKind::OutputParser),
            ],
            edges: vec![
                edge_on("t", "main", "left"),
                edge_on("t", "main", "right"),
                edge_on("left", "main", "m"),
                edge_on("right", "main", "m"),
                edge_on("m", "main", "l"),
                edge_on("l", "body", "work"),
                edge_on("work", "main", "l"),
            ],
            ..Default::default()
        };
        assert_eq!(validate_all(&graph), Vec::new());
    }

    /// A cycle with no `loop` node is legal as long as the trigger declares a
    /// `recursion_limit` — the bound just has to come from somewhere.
    #[test]
    fn a_recursion_limit_bounds_a_loopless_cycle() {
        let graph = WorkflowGraph {
            nodes: vec![
                node_cfg(
                    "t",
                    NodeKind::Trigger,
                    serde_json::json!({ "recursion_limit": 10 }),
                ),
                node("a", NodeKind::OutputParser),
                node("b", NodeKind::OutputParser),
            ],
            edges: vec![
                edge_on("t", "main", "a"),
                edge_on("a", "main", "b"),
                edge_on("b", "main", "a"),
            ],
            ..Default::default()
        };
        assert_eq!(validate_all(&graph), Vec::new());
    }
}
