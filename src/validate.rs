//! Structural validation for [`WorkflowGraph`]s, run before compilation.

use std::collections::HashSet;

use serde_json::Value;

use crate::error::ValidationError;
use crate::model::{NodeKind, WorkflowGraph};

/// Validates a workflow graph's structure.
///
/// Currently checks: unique node ids, exactly one trigger node, that every edge
/// references existing nodes, no duplicate edges, and per-node `on_error` policy
/// sanity (a known value, and an `error` edge when the policy is `route`).
/// Cycle-legality and per-kind configuration checks are completed in stages
/// A1–A2.
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
/// `on_error` policy → per-kind config → condition routing), and every error is
/// self-contained (no check can panic on a graph that failed an earlier one),
/// so accumulating is safe. The first element is identical to what
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

    errors
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
