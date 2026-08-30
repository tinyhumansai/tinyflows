//! The ancestor gate: which repairs may skip a committed prefix.
//!
//! Every case here is a way of getting this wrong that would not fail loudly.
//! A run that continues on a prefix the new graph would not have produced
//! completes, reports success, and is wrong about it — so the tests are
//! written from the unsafe side, and the safe cases exist to prove the gate is
//! not simply refusing everything.

use serde_json::json;
use tinyflows::graph_ops::GraphOp;
use tinyflows::model::{Edge, Node, NodeKind, WorkflowGraph, WorkflowInput};

use super::may_continue;

/// `start → fetch → review → post`. `review` is where these runs break: it has
/// a real upstream to protect and a real downstream that has not run.
fn graph() -> WorkflowGraph {
    chain(&["start", "fetch", "review", "post"])
}

fn chain(ids: &[&str]) -> WorkflowGraph {
    let nodes = ids
        .iter()
        .map(|id| Node {
            id: (*id).to_string(),
            kind: if *id == "start" {
                NodeKind::Trigger
            } else {
                NodeKind::Agent
            },
            type_version: 1,
            name: (*id).to_string(),
            config: json!({}),
            ports: Vec::new(),
            position: None,
        })
        .collect();
    let edges = ids
        .windows(2)
        .map(|pair| Edge {
            from_node: pair[0].to_string(),
            from_port: "main".to_string(),
            to_node: pair[1].to_string(),
            to_port: "main".to_string(),
        })
        .collect();
    WorkflowGraph {
        schema_version: 1,
        id: None,
        name: "review".to_string(),
        inputs: Vec::new(),
        agents: Vec::new(),
        nodes,
        edges,
    }
}

fn edit(id: &str) -> GraphOp {
    GraphOp::UpdateNodeConfig {
        id: id.to_string(),
        config: json!({ "prompt": "=.item.text" }),
    }
}

#[test]
fn editing_the_node_that_failed_may_continue() {
    // The case worth having. A continue re-runs the failed node, so its new
    // config is read fresh — and the prefix that produced its input is
    // untouched.
    assert!(may_continue(
        &graph(),
        &graph(),
        "review",
        &[edit("review")]
    ));
}

#[test]
fn editing_something_downstream_may_continue() {
    // Nothing downstream of the failure has run, so nothing about it can
    // conflict with what is committed.
    assert!(may_continue(&graph(), &graph(), "review", &[edit("post")]));
}

#[test]
fn editing_an_upstream_node_must_start_over() {
    // `fetch` already ran and its output is committed. Continuing would run
    // the repaired `review` against the *old* fetch's output while the store
    // says the graph is the new one — green, and wrong about it.
    assert!(!may_continue(
        &graph(),
        &graph(),
        "review",
        &[edit("fetch")]
    ));
}

#[test]
fn editing_the_trigger_must_start_over() {
    // The trigger is an ancestor like any other, and its output seeds
    // everything.
    assert!(!may_continue(
        &graph(),
        &graph(),
        "review",
        &[edit("start")]
    ));
}

#[test]
fn a_repair_that_adds_an_upstream_node_must_start_over() {
    // The dangerous direction, and the one a parent-only ancestor set misses:
    // the new node is not an ancestor in the graph that RAN, only in the
    // repaired one. Continuing would re-enter `review` with the upstream it
    // was just given still missing — so the fix would look like it had not
    // worked, and the next repair would chase the wrong thing.
    let child = chain(&["start", "fetch", "enrich", "review", "post"]);
    let ops = vec![
        GraphOp::AddNode {
            node: Node {
                id: "enrich".to_string(),
                kind: NodeKind::Agent,
                type_version: 1,
                name: "enrich".to_string(),
                config: json!({}),
                ports: Vec::new(),
                position: None,
            },
        },
        GraphOp::AddEdge {
            edge: Edge {
                from_node: "enrich".to_string(),
                from_port: "main".to_string(),
                to_node: "review".to_string(),
                to_port: "main".to_string(),
            },
        },
    ];
    assert!(!may_continue(&graph(), &child, "review", &ops));
}

#[test]
fn a_repair_that_removes_an_upstream_node_must_start_over() {
    // The other direction: `fetch` is an ancestor only in the graph that ran.
    let child = chain(&["start", "review", "post"]);
    let ops = vec![GraphOp::RemoveNode {
        id: "fetch".to_string(),
    }];
    assert!(!may_continue(&graph(), &child, "review", &ops));
}

#[test]
fn rewiring_an_edge_out_of_an_ancestor_must_start_over() {
    // An edge is a statement about two nodes. Changing what an ancestor feeds
    // changes that ancestor's meaning as surely as editing its config, and
    // only one endpoint has to be upstream for that to bite.
    let ops = vec![GraphOp::RemoveEdge {
        from_node: "fetch".to_string(),
        from_port: "main".to_string(),
        to_node: "review".to_string(),
        to_port: "main".to_string(),
    }];
    assert!(!may_continue(&graph(), &graph(), "review", &ops));
}

#[test]
fn changing_the_declared_inputs_must_start_over() {
    // Declared values are read by expressions anywhere, including in the
    // prefix that already ran, so this invalidates every committed output at
    // once — even though it names no node.
    let ops = vec![GraphOp::SetWorkflowInputs {
        inputs: vec![WorkflowInput::new(
            "repo".to_string(),
            tinyflows::model::InputType::String,
        )],
    }];
    assert!(!may_continue(&graph(), &graph(), "review", &ops));
}

#[test]
fn a_failed_node_the_graph_does_not_have_must_start_over() {
    // Something is out of step — a stale point, the wrong thread — and
    // guessing is the one thing this must not do.
    assert!(!may_continue(&graph(), &graph(), "nonexistent", &[]));
    assert!(!may_continue(&graph(), &graph(), "", &[]));
}

#[test]
fn a_failure_in_the_first_step_may_continue_whatever_was_edited() {
    // Not a special case, a consequence: a node with no ancestors has no
    // committed prefix, so there is nothing an edit could invalidate. The
    // continue saves nothing here, and is still correct.
    assert!(may_continue(&graph(), &graph(), "start", &[edit("post")]));
    assert!(may_continue(&graph(), &graph(), "start", &[edit("start")]));
}

#[test]
fn a_diamond_protects_both_branches() {
    // Ancestry is transitive and not a straight line. `left` and `right` both
    // feed `join`, and an edit to either invalidates what `join` was given.
    let mut graph = chain(&["start", "join", "post"]);
    for id in ["left", "right"] {
        graph.nodes.push(Node {
            id: id.to_string(),
            kind: NodeKind::Agent,
            type_version: 1,
            name: id.to_string(),
            config: json!({}),
            ports: Vec::new(),
            position: None,
        });
        graph.edges.push(Edge {
            from_node: "start".to_string(),
            from_port: "main".to_string(),
            to_node: id.to_string(),
            to_port: "main".to_string(),
        });
        graph.edges.push(Edge {
            from_node: id.to_string(),
            from_port: "main".to_string(),
            to_node: "join".to_string(),
            to_port: "main".to_string(),
        });
    }
    assert!(!may_continue(&graph, &graph, "join", &[edit("left")]));
    assert!(!may_continue(&graph, &graph, "join", &[edit("right")]));
    assert!(
        may_continue(&graph, &graph, "join", &[edit("post")]),
        "the node after the join still has not run"
    );
}

#[test]
fn a_cycle_upstream_does_not_hang_the_walk() {
    // Loop nodes are closed by a back-edge, so the ancestor walk meets cycles
    // in ordinary graphs. Seen-set termination, asserted rather than assumed.
    let mut graph = chain(&["start", "head", "body", "review"]);
    graph.edges.push(Edge {
        from_node: "body".to_string(),
        from_port: "main".to_string(),
        to_node: "head".to_string(),
        to_port: "main".to_string(),
    });
    assert!(!may_continue(&graph, &graph, "review", &[edit("body")]));
    assert!(may_continue(&graph, &graph, "review", &[edit("review")]));
}
