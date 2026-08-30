//! Tests for the engine-compatibility refusal.
//!
//! This gate fails closed, which makes its false-positive obligation the sharp
//! one: it refuses a graph outright, so every shape it *accepts* is a shape a
//! user can still save. The accepting cases below are therefore not padding —
//! each is a topology that is fine and that a coarser rule ("any fan-in behind
//! any condition") would have taken away.

use serde_json::json;

use super::*;

fn graph(value: serde_json::Value) -> WorkflowGraph {
    serde_json::from_value(value).expect("graph parses")
}

/// The refused shape: `a` reaches the fan-in from behind two branching
/// decisions, so the relief picks only the outer one and either fires early —
/// dropping `a`'s data — or never fires at all.
#[test]
fn a_fan_in_predecessor_behind_two_branchers_is_refused() {
    let g = graph(json!({
        "name": "nested-conditional-fan-in",
        "nodes": [
            { "id": "start", "kind": "trigger", "name": "Trigger" },
            { "id": "outer", "kind": "condition", "name": "Outer", "config": { "field": "o" } },
            { "id": "inner", "kind": "condition", "name": "Inner", "config": { "field": "i" } },
            { "id": "outer_else", "kind": "output_parser", "name": "Outer else" },
            { "id": "inner_else", "kind": "output_parser", "name": "Inner else" },
            { "id": "a", "kind": "output_parser", "name": "A" },
            { "id": "c", "kind": "output_parser", "name": "C" },
            { "id": "m", "kind": "merge", "name": "Merge" }
        ],
        "edges": [
            { "from_node": "start", "from_port": "main", "to_node": "outer" },
            { "from_node": "start", "from_port": "main", "to_node": "c" },
            { "from_node": "outer", "from_port": "true", "to_node": "inner" },
            { "from_node": "outer", "from_port": "false", "to_node": "outer_else" },
            { "from_node": "inner", "from_port": "true", "to_node": "a" },
            { "from_node": "inner", "from_port": "false", "to_node": "inner_else" },
            { "from_node": "a", "from_port": "main", "to_node": "m" },
            { "from_node": "c", "from_port": "main", "to_node": "m" }
        ]
    }));

    let errors = errors(&g);
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].code, UNSUPPORTED_NESTED_CONDITIONAL_FAN_IN);
    assert_eq!(errors[0].node_id.as_deref(), Some("m"));
}

/// One brancher is exactly what the relief handles, so this must stay savable.
#[test]
fn a_fan_in_predecessor_behind_one_brancher_is_accepted() {
    let g = graph(json!({
        "name": "one-level-mixed-fan-in",
        "nodes": [
            { "id": "start", "kind": "trigger", "name": "Trigger" },
            { "id": "cond", "kind": "condition", "name": "Condition", "config": { "field": "f" } },
            { "id": "a", "kind": "output_parser", "name": "A" },
            { "id": "other", "kind": "output_parser", "name": "Other" },
            { "id": "c", "kind": "output_parser", "name": "C" },
            { "id": "m", "kind": "merge", "name": "Merge" }
        ],
        "edges": [
            { "from_node": "start", "from_port": "main", "to_node": "cond" },
            { "from_node": "start", "from_port": "main", "to_node": "c" },
            { "from_node": "cond", "from_port": "true", "to_node": "a" },
            { "from_node": "cond", "from_port": "false", "to_node": "other" },
            { "from_node": "a", "from_port": "main", "to_node": "m" },
            { "from_node": "c", "from_port": "main", "to_node": "m" }
        ]
    }));

    assert!(errors(&g).is_empty(), "{:?}", errors(&g));
}

/// Nesting on its own is not the problem — an unrelieved fan-in is. Without one
/// there is nothing for the lowering to get wrong.
#[test]
fn nesting_without_a_fan_in_is_accepted() {
    let g = graph(json!({
        "name": "nested-without-fan-in",
        "nodes": [
            { "id": "start", "kind": "trigger", "name": "Trigger" },
            { "id": "outer", "kind": "condition", "name": "Outer", "config": { "field": "o" } },
            { "id": "inner", "kind": "condition", "name": "Inner", "config": { "field": "i" } },
            { "id": "outer_else", "kind": "output_parser", "name": "Outer else" },
            { "id": "a", "kind": "output_parser", "name": "A" },
            { "id": "inner_else", "kind": "output_parser", "name": "Inner else" }
        ],
        "edges": [
            { "from_node": "start", "from_port": "main", "to_node": "outer" },
            { "from_node": "outer", "from_port": "true", "to_node": "inner" },
            { "from_node": "outer", "from_port": "false", "to_node": "outer_else" },
            { "from_node": "inner", "from_port": "true", "to_node": "a" },
            { "from_node": "inner", "from_port": "false", "to_node": "inner_else" }
        ]
    }));

    assert!(errors(&g).is_empty(), "{:?}", errors(&g));
}

/// A predecessor reachable from the trigger by `main`-only edges is
/// unconditional: it always runs, so the barrier never needs relieving.
#[test]
fn an_unconditional_fan_in_is_accepted() {
    let g = graph(json!({
        "name": "unconditional-fan-in",
        "nodes": [
            { "id": "start", "kind": "trigger", "name": "Trigger" },
            { "id": "a", "kind": "output_parser", "name": "A" },
            { "id": "c", "kind": "output_parser", "name": "C" },
            { "id": "m", "kind": "merge", "name": "Merge" }
        ],
        "edges": [
            { "from_node": "start", "from_port": "main", "to_node": "a" },
            { "from_node": "start", "from_port": "main", "to_node": "c" },
            { "from_node": "a", "from_port": "main", "to_node": "m" },
            { "from_node": "c", "from_port": "main", "to_node": "m" }
        ]
    }));

    assert!(errors(&g).is_empty(), "{:?}", errors(&g));
}

/// An inline child is part of the graph and is walked, with the refusal
/// attributed to the node that carries it — otherwise the author is told a node
/// id that appears nowhere in the file they are editing.
#[test]
fn an_inline_sub_workflow_child_is_walked_and_its_refusal_is_attributed() {
    let child = json!({
        "name": "child",
        "nodes": [
            { "id": "start", "kind": "trigger", "name": "Trigger" },
            { "id": "outer", "kind": "condition", "name": "Outer", "config": { "field": "o" } },
            { "id": "inner", "kind": "condition", "name": "Inner", "config": { "field": "i" } },
            { "id": "outer_else", "kind": "output_parser", "name": "Outer else" },
            { "id": "inner_else", "kind": "output_parser", "name": "Inner else" },
            { "id": "a", "kind": "output_parser", "name": "A" },
            { "id": "c", "kind": "output_parser", "name": "C" },
            { "id": "m", "kind": "merge", "name": "Merge" }
        ],
        "edges": [
            { "from_node": "start", "from_port": "main", "to_node": "outer" },
            { "from_node": "start", "from_port": "main", "to_node": "c" },
            { "from_node": "outer", "from_port": "true", "to_node": "inner" },
            { "from_node": "outer", "from_port": "false", "to_node": "outer_else" },
            { "from_node": "inner", "from_port": "true", "to_node": "a" },
            { "from_node": "inner", "from_port": "false", "to_node": "inner_else" },
            { "from_node": "a", "from_port": "main", "to_node": "m" },
            { "from_node": "c", "from_port": "main", "to_node": "m" }
        ]
    });
    let g = graph(json!({
        "name": "parent",
        "nodes": [
            { "id": "start", "kind": "trigger", "name": "Trigger" },
            { "id": "call", "kind": "sub_workflow", "name": "Call",
              "config": { "workflow": child } }
        ],
        "edges": [{ "from_node": "start", "from_port": "main", "to_node": "call" }]
    }));

    let errors = errors(&g);
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(
        errors[0]
            .message
            .starts_with("Inline sub_workflow node 'call'"),
        "{}",
        errors[0].message
    );
}

/// The depth budget is the run's, not the child's, which is why it can be
/// passed in: a host resolving a *saved* child mid-chain has to check it to the
/// remaining depth the root allows.
#[test]
fn a_zero_depth_budget_stops_the_walk_before_any_inline_child() {
    let child = json!({
        "name": "child",
        "nodes": [
            { "id": "start", "kind": "trigger", "name": "Trigger" },
            { "id": "outer", "kind": "condition", "name": "Outer", "config": { "field": "o" } },
            { "id": "inner", "kind": "condition", "name": "Inner", "config": { "field": "i" } },
            { "id": "outer_else", "kind": "output_parser", "name": "Outer else" },
            { "id": "inner_else", "kind": "output_parser", "name": "Inner else" },
            { "id": "a", "kind": "output_parser", "name": "A" },
            { "id": "c", "kind": "output_parser", "name": "C" },
            { "id": "m", "kind": "merge", "name": "Merge" }
        ],
        "edges": [
            { "from_node": "start", "from_port": "main", "to_node": "outer" },
            { "from_node": "start", "from_port": "main", "to_node": "c" },
            { "from_node": "outer", "from_port": "true", "to_node": "inner" },
            { "from_node": "outer", "from_port": "false", "to_node": "outer_else" },
            { "from_node": "inner", "from_port": "true", "to_node": "a" },
            { "from_node": "inner", "from_port": "false", "to_node": "inner_else" },
            { "from_node": "a", "from_port": "main", "to_node": "m" },
            { "from_node": "c", "from_port": "main", "to_node": "m" }
        ]
    });
    let g = graph(json!({
        "name": "parent",
        "nodes": [
            { "id": "start", "kind": "trigger", "name": "Trigger" },
            { "id": "call", "kind": "sub_workflow", "name": "Call",
              "config": { "workflow": child } }
        ],
        "edges": [{ "from_node": "start", "from_port": "main", "to_node": "call" }]
    }));

    assert!(errors_with_max_depth(&g, 0).is_empty());
    assert_eq!(errors_with_max_depth(&g, 1).len(), 1);
}

/// The cap comes off the trigger when it declares one, so a graph that
/// legitimately nests deeper is still checked all the way down.
#[test]
fn the_depth_budget_is_read_off_the_trigger() {
    let g = graph(json!({
        "name": "declares-a-cap",
        "nodes": [
            { "id": "start", "kind": "trigger", "name": "Trigger",
              "config": { "max_sub_workflow_depth": 9 } }
        ],
        "edges": []
    }));
    assert_eq!(max_sub_workflow_depth(&g), 9);

    let bare = graph(json!({
        "name": "declares-nothing",
        "nodes": [{ "id": "start", "kind": "trigger", "name": "Trigger" }],
        "edges": []
    }));
    assert_eq!(
        max_sub_workflow_depth(&bare),
        crate::engine::MAX_SUB_WORKFLOW_DEPTH
    );
}
