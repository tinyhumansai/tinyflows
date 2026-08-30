//! Policy predicates over a [`WorkflowGraph`] — the questions a host has to
//! answer *before* it saves or runs a graph, and whose answers do not depend on
//! which host is asking.
//!
//! Two safety rules live here, and both exist because the honest default for a
//! freshly authored workflow is "does nothing until a human says so":
//!
//! 1. **A graph that fires unattended must not save itself armed.**
//!    [`trigger_is_automatic`] says whether a graph's trigger can fire without
//!    anyone asking it to; a host uses that to persist `enabled: false` until
//!    the author arms it explicitly.
//! 2. **A graph that can act on the world must require approval.**
//!    [`enforce_side_effect_approval`] overrides a caller's `require_approval:
//!    false` when [`graph_has_outbound_side_effect`] holds, on create *and* on
//!    a later edit that adds such a node to a previously read-only graph.
//!
//! [`graph_has_actionable_nodes`] is a third, quieter check: whether a graph
//! has anything to do at all, so a host can say so instead of reporting a
//! successful run that did nothing.
//!
//! What is *not* here is anything about which trigger kinds a particular host
//! has actually wired to a dispatcher. "This host does not deliver webhooks
//! yet" is a fact about that host and belongs in its own overlay.

use tinyflows::model::{Node, NodeKind, TriggerKind, WorkflowGraph};

/// Whether `graph`'s trigger fires **without a human in the loop** — i.e. on
/// a timer, an inbound webhook, or a connected-app event, as opposed to
/// `manual` (only ever fired by an explicit `flows_run`). Used by
/// a host to decide whether a freshly-saved flow may persist `enabled: true`
/// or must persist `enabled: false` until the author arms it explicitly.
///
/// Deliberately broader than "which kinds does this host dispatch today":
/// a host that has not wired webhooks yet WILL fire them unattended the moment
/// it does, so a webhook-trigger flow must not be handed to the author
/// pre-armed either. Returns `false` for a graph with no single
/// resolvable trigger node or no `trigger_kind` discriminator (never a
/// surprise — it never self-fires).
pub fn trigger_is_automatic(graph: &WorkflowGraph) -> bool {
    let Some(trigger) = graph.trigger() else {
        return false;
    };
    let Some(kind_value) = trigger.config.get("trigger_kind") else {
        return false;
    };
    let Ok(kind) = serde_json::from_value::<TriggerKind>(kind_value.clone()) else {
        return false;
    };
    matches!(
        kind,
        TriggerKind::Schedule | TriggerKind::AppEvent | TriggerKind::Webhook
    )
}

/// Whether `graph` contains a node that can produce a real outbound side
/// effect — `tool_call` (a curated integration action), `http_request`,
/// `code` (sandboxed but Turing-complete, can reach the network), `shell`
/// (an author-supplied POSIX script run through the host capability, which
/// can modify files, invoke programs, or reach the network same as `code`),
/// or an `agent` node that can itself invoke a tool. Used by a host to force
/// `require_approval: true` on any graph that can act on the world,
/// regardless of what the caller passed. A graph built only from `trigger` /
/// data-flow / read-only `agent` nodes is unaffected.
pub fn graph_has_outbound_side_effect(graph: &WorkflowGraph) -> bool {
    has_outbound_side_effect_to_depth(graph, tinyflows::engine::MAX_SUB_WORKFLOW_DEPTH)
}

/// [`graph_has_outbound_side_effect`], bounded so a pathologically nested or
/// self-referencing inline chain cannot recurse forever.
///
/// A `sub_workflow` hides its work behind a single node, and this rule decides
/// whether a flow may ever run unattended — so not looking inside one is how a
/// graph reading `trigger → sub_workflow` saves with no approval gate while its
/// child sends the email. Both forms the node accepts are covered:
///
/// - **`workflow`** — an inline child graph, descended into to `depth`.
/// - **`workflow_id`** — a reference to a *saved* workflow, which counts as a
///   side effect on sight. This crate has no catalog and cannot see what it
///   names, so the honest answer is "possibly".
///
/// The two costs are not symmetric, which is why this fails closed where the
/// authoring gates do the opposite: a false positive here asks a human to
/// approve a run that did not need it, while a false negative lets an
/// unreviewed workflow act on the world. A gate that *refuses* a graph must
/// only fire on what is certain; a rule that merely *requires a human* should
/// not. An unparseable child and an exhausted budget are treated the same way.
fn has_outbound_side_effect_to_depth(graph: &WorkflowGraph, depth: u64) -> bool {
    graph.nodes.iter().any(|n| {
        if matches!(
            n.kind,
            NodeKind::ToolCall | NodeKind::HttpRequest | NodeKind::Code | NodeKind::Shell
        ) || (n.kind == NodeKind::Agent && node_agent_has_tool_grant(graph, n))
        {
            return true;
        }
        if n.kind != NodeKind::SubWorkflow {
            return false;
        }
        if n.config.get("workflow_id").is_some() {
            return true;
        }
        let Some(inline) = n.config.get("workflow") else {
            return false;
        };
        if depth == 0 {
            // Out of budget with a child still unexamined. Same fail-closed
            // reasoning as an unresolvable `workflow_id`.
            return true;
        }
        match serde_json::from_value::<WorkflowGraph>(inline.clone()) {
            Ok(child) => has_outbound_side_effect_to_depth(&child, depth - 1),
            // A child this crate cannot parse is one it cannot clear.
            Err(_) => true,
        }
    })
}

/// Whether an `agent` node `n` can invoke a tool at all — the inline
/// `config.tools` grant it carries itself, or (when it names an
/// `agent_ref`) the grants on the [`AgentDefinition`] that ref resolves to in
/// this graph's own registry.
///
/// Deliberately conservative: an `agent_ref` this graph does not define is
/// resolved by the host at run time and may carry tool grants this crate cannot
/// see. Requiring approval is safer than clearing an opaque host definition.
fn node_agent_has_tool_grant(graph: &WorkflowGraph, n: &Node) -> bool {
    let inline_tools = n
        .config
        .get("tools")
        .and_then(|v| v.as_array())
        .is_some_and(|a| !a.is_empty());
    if inline_tools {
        return true;
    }
    let Some(agent_ref) = n.config.get("agent_ref").and_then(|v| v.as_str()) else {
        return false;
    };
    graph
        .agent(agent_ref)
        .is_none_or(|def| !def.tools.is_empty())
}

/// Shared side-effect enforcement: forces `require_approval` to `true` when `graph` contains an
/// outbound side-effect node, no matter what the caller asked for. Used by both
/// the create and the update paths so a flow can never persist
/// `require_approval: false` alongside a `tool_call` / `http_request` / `code`
/// node — on create OR on a later edit that *adds* such a node to a
/// previously-read-only graph.
///
/// Returns `(effective_require_approval, was_forced)`: `was_forced` is `true`
/// only when the caller's own toggle was `false` but a side-effect node
/// required the override — callers use it to decide whether to emit the
/// loud "forced to true" log/result note.
pub fn enforce_side_effect_approval(
    graph: &WorkflowGraph,
    caller_require_approval: bool,
) -> (bool, bool) {
    let has_side_effect = graph_has_outbound_side_effect(graph);
    let effective_require_approval = caller_require_approval || has_side_effect;
    let was_forced = has_side_effect && !caller_require_approval;
    (effective_require_approval, was_forced)
}

/// Whether `graph` has anything for a run to actually *do* — i.e. at
/// least one non-`trigger` node **reachable from the trigger** by following
/// directed edges. A graph made of nothing but a bare `trigger` node (or a
/// `trigger` plus unreachable/disconnected nodes — even ones wired to each
/// other by their own edges, just not to the trigger) can compile and "run"
/// cleanly while producing no work whatsoever — the exact live finding this
/// guards: a trigger-only flow reported `status="completed"
/// pending_approvals=0` having done nothing, which reads as a successful
/// automation to anyone not staring at the node count. A host uses it to attach a human-readable note to an otherwise-silent "success".
///
/// Deliberately a reachability walk rather than "any edge at all exists":
/// `nodes.len() > 1 && !edges.is_empty()` would count a disconnected
/// component's internal edges as actionable even though nothing downstream
/// of the trigger ever runs.
pub fn graph_has_actionable_nodes(graph: &WorkflowGraph) -> bool {
    let Some(trigger) = graph.trigger() else {
        // No single resolvable trigger to walk from — fall back to the
        // coarse "any non-trigger node wired up by an edge" check so a
        // malformed/ambiguous-trigger graph doesn't spuriously suppress the
        // empty-flow note.
        return graph.nodes.iter().any(|n| n.kind != NodeKind::Trigger) && !graph.edges.is_empty();
    };

    let mut visited: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut stack = vec![trigger.id.as_str()];
    while let Some(current) = stack.pop() {
        if !visited.insert(current) {
            continue;
        }
        for next in graph.successors(current) {
            if !visited.contains(next) {
                stack.push(next);
            }
        }
    }

    visited
        .into_iter()
        .filter_map(|id| graph.node(id))
        .any(|n| n.kind != NodeKind::Trigger)
}

#[cfg(test)]
#[path = "graph_policy_tests.rs"]
mod tests;
