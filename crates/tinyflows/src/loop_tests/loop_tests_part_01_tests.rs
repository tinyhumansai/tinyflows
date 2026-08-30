

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
