// `void` topology rules. The kind asserts exactly one thing — "the branch ends
// here, deliberately" — so these pin the two ways to contradict it, the
// `on_error` interaction, and the one place the rule had to be *relaxed*
// (a lane branch may end in a void) without opening the hole it was guarding.

use serde_json::json;

fn void_edge(from: &str, to: &str) -> Edge {
    Edge {
        from_node: from.to_string(),
        from_port: "main".to_string(),
        to_node: to.to_string(),
        to_port: "main".to_string(),
    }
}

/// `t -> v(void)` — the minimal legal void.
fn graph_with_void() -> WorkflowGraph {
    WorkflowGraph {
        nodes: vec![node("t", NodeKind::Trigger), node("v", NodeKind::Void)],
        edges: vec![void_edge("t", "v")],
        ..Default::default()
    }
}

#[test]
fn void_as_a_leaf_is_accepted() {
    assert_eq!(validate(&graph_with_void()), Ok(()));
}

#[test]
fn void_with_an_outgoing_edge_is_rejected() {
    let mut graph = graph_with_void();
    graph.nodes.push(node("x", NodeKind::Transform));
    graph.edges.push(void_edge("v", "x"));

    let errors = validate_all(&graph);
    let reason = errors
        .iter()
        .find_map(|e| match e {
            ValidationError::InvalidNodeConfig { node, reason } if node == "v" => Some(reason),
            _ => None,
        })
        .expect("void with an outgoing edge should be rejected");
    assert!(
        reason.contains("terminal sink") && reason.contains("\"x\""),
        "the message should name the offending target: {reason}"
    );
}

#[test]
fn void_with_several_outgoing_edges_names_them_deterministically() {
    // The message lists targets, so it has to be stable across runs — `errors`
    // is a Vec an author reads, not a set.
    let mut graph = graph_with_void();
    graph.nodes.push(node("b", NodeKind::Transform));
    graph.nodes.push(node("a", NodeKind::Transform));
    graph.edges.push(void_edge("v", "b"));
    graph.edges.push(void_edge("v", "a"));

    let errors = validate_all(&graph);
    let reason = errors
        .iter()
        .find_map(|e| match e {
            ValidationError::InvalidNodeConfig { node, reason } if node == "v" => Some(reason),
            _ => None,
        })
        .expect("rejected");
    assert!(reason.contains("[\"a\", \"b\"]"), "{reason}");
}

#[test]
fn void_with_no_incoming_edge_is_rejected() {
    let graph = WorkflowGraph {
        nodes: vec![node("t", NodeKind::Trigger), node("v", NodeKind::Void)],
        edges: vec![],
        ..Default::default()
    };

    let errors = validate_all(&graph);
    assert!(
        errors.iter().any(|e| matches!(
            e,
            ValidationError::InvalidNodeConfig { node, reason }
                if node == "v" && reason.contains("no incoming edge")
        )),
        "an orphan void declares nothing: {errors:?}"
    );
}

#[test]
fn void_with_on_error_route_is_rejected_without_demanding_an_error_edge() {
    // The whole point of the special case: `MissingErrorRoute` would tell the
    // author to add an edge the void rule then rejects.
    let mut graph = graph_with_void();
    graph.nodes[1].config = json!({ "on_error": "route" });

    let errors = validate_all(&graph);
    assert!(
        !errors
            .iter()
            .any(|e| matches!(e, ValidationError::MissingErrorRoute(_))),
        "must not ask for an `error` edge it would then refuse: {errors:?}"
    );
    assert!(
        errors.iter().any(|e| matches!(
            e,
            ValidationError::InvalidNodeConfig { node, reason }
                if node == "v" && reason.contains("nowhere to route to")
        )),
        "{errors:?}"
    );
}

#[test]
fn void_accepts_on_error_stop_and_continue() {
    for policy in ["stop", "continue"] {
        let mut graph = graph_with_void();
        graph.nodes[1].config = json!({ "on_error": policy });
        assert_eq!(validate(&graph), Ok(()), "on_error {policy} should be legal");
    }
}

#[test]
fn execution_is_still_rejected_on_void() {
    // Pins void out of the mapping kinds: it consumes a batch, it does not map
    // over one.
    let mut graph = graph_with_void();
    graph.nodes[1].config = json!({ "execution": "per_item" });

    let errors = validate_all(&graph);
    assert!(
        errors.iter().any(|e| matches!(
            e,
            ValidationError::InvalidNodeConfig { node, reason }
                if node == "v" && reason.contains("void")
        )),
        "{errors:?}"
    );
}

// --- scatter lane interaction ---------------------------------------------

/// `t -> fan(scatter) -> work -> collect(gather)`, plus an optional side branch
/// hanging off `work` whose nodes are appended by the caller.
fn lane_graph(extra_nodes: Vec<Node>, extra_edges: Vec<Edge>) -> WorkflowGraph {
    let mut nodes = vec![
        node("t", NodeKind::Trigger),
        node("fan", NodeKind::Scatter),
        node("work", NodeKind::Transform),
        node("collect", NodeKind::Gather),
    ];
    nodes[3].config = json!({ "from": ["work"] });
    nodes.extend(extra_nodes);

    let mut edges = vec![
        void_edge("t", "fan"),
        void_edge("fan", "work"),
        void_edge("work", "collect"),
    ];
    edges.extend(extra_edges);

    WorkflowGraph {
        nodes,
        edges,
        ..Default::default()
    }
}

#[test]
fn a_lane_side_branch_may_end_in_void() {
    // The primary use case: fire-and-forget inside a lane, which was previously
    // impossible to express because every lane node had to reach the gather.
    let graph = lane_graph(
        vec![node("notify", NodeKind::Transform), node("v", NodeKind::Void)],
        vec![void_edge("work", "notify"), void_edge("notify", "v")],
    );
    assert_eq!(validate(&graph), Ok(()));
}

#[test]
fn a_lane_side_branch_that_dead_ends_without_a_void_is_still_rejected() {
    // Same shape, minus the void: still an accident, still refused.
    let graph = lane_graph(
        vec![node("notify", NodeKind::Transform)],
        vec![void_edge("work", "notify")],
    );

    let errors = validate_all(&graph);
    assert!(
        errors.iter().any(|e| matches!(
            e,
            ValidationError::InvalidNodeConfig { node, reason }
                if node == "notify" && reason.contains("no path onward to a `gather`")
        )),
        "{errors:?}"
    );
}

#[test]
fn a_scatter_whose_only_path_ends_in_void_is_still_rejected() {
    // The hole the relaxation must not open. A scatter with no gather anywhere
    // is a plain fan-out wearing a scatter costume, and a void downstream does
    // not make it one — `region_members` yields nothing, so the "no `gather`
    // downstream" error still fires.
    let graph = WorkflowGraph {
        nodes: vec![
            node("t", NodeKind::Trigger),
            node("fan", NodeKind::Scatter),
            node("work", NodeKind::Transform),
            node("v", NodeKind::Void),
        ],
        edges: vec![
            void_edge("t", "fan"),
            void_edge("fan", "work"),
            void_edge("work", "v"),
        ],
        ..Default::default()
    };

    let errors = validate_all(&graph);
    assert!(
        errors.iter().any(|e| matches!(
            e,
            ValidationError::InvalidNodeConfig { node, reason }
                if node == "fan" && reason.contains("no `gather` downstream")
        )),
        "{errors:?}"
    );
}
