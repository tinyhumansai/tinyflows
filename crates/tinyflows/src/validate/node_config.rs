//! Per-kind node **config** validation.
//!
//! The half of [`validate_all`](super::validate_all) that reads a node's
//! `config` object rather than the graph's shape: the `sub_workflow` child
//! reference, per-item fan-out selectors, `memory` scope rules, the `dedup`
//! key, and the `approval` enums. Every check here is about one node in
//! isolation — nothing in this module looks at an edge — which is what makes it
//! a module of its own rather than an arbitrary cut through `validate_all`.

use serde_json::Value;

use crate::error::ValidationError;
use crate::model::{NodeKind, WorkflowGraph};

use super::kind_name;

/// Appends every per-kind config error the graph carries, in node order.
pub(super) fn validate_node_configs(graph: &WorkflowGraph, errors: &mut Vec<ValidationError>) {
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

    // `approval` node config. These are all closed enums that SELECT BEHAVIOUR,
    // so a typo cannot be caught at run time without silently changing what the
    // node does: a misspelled `on_reject` would quietly route a rejection that
    // was meant to fail the run, and a misspelled `wait_mode` would quietly
    // suspend a review the author wanted polled. Refuse them at the door, where
    // the message can name the node and the alternatives.
    for node in &graph.nodes {
        if node.kind != NodeKind::Approval {
            continue;
        }

        for (key, allowed) in [
            ("wait_mode", &["suspend", "poll"][..]),
            ("on_reject", &["route", "error", "drop"][..]),
            ("on_timeout", &["error", "reject", "route"][..]),
        ] {
            let Some(value) = node.config.get(key) else {
                continue;
            };
            if !value.as_str().is_some_and(|v| allowed.contains(&v)) {
                errors.push(ValidationError::InvalidNodeConfig {
                    node: node.id.clone(),
                    reason: format!(
                        "approval node has unknown `{key}` {value} (expected one of {})",
                        allowed
                            .iter()
                            .map(|v| format!("{v:?}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                });
            }
        }

        // Reviewer handles are opaque to the crate, but their *shape* is not:
        // a bare string here (the natural mistake for a single reviewer) would
        // be read as "nobody", and the review would go to an empty audience
        // with no error anywhere. An empty array reaches the same audience of
        // nobody just as silently, so it is refused for the same reason.
        if let Some(assignees) = node.config.get("assignees") {
            if !assignees
                .as_array()
                .is_some_and(|values| !values.is_empty() && values.iter().all(Value::is_string))
            {
                errors.push(ValidationError::InvalidNodeConfig {
                    node: node.id.clone(),
                    reason: "approval node `assignees` must be a non-empty array of strings (a \
                             single reviewer is a one-element array)"
                        .to_string(),
                });
            }
        }
    }
}
