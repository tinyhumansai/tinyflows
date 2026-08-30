
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
