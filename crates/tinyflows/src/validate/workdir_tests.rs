//! Tests for the working-directory config check: a key that names where a step
//! runs must be one the node actually reads.

use serde_json::json;

use crate::model::WorkflowGraph;
use crate::validate::validate_all;

/// A one-node graph whose single non-trigger node carries `config`.
fn graph_with(kind: &str, config: serde_json::Value) -> WorkflowGraph {
    serde_json::from_value(json!({
        "nodes": [
            { "id": "t", "kind": "trigger", "name": "start", "config": { "trigger_kind": "manual" } },
            { "id": "step", "kind": kind, "name": "step", "config": config }
        ],
        "edges": [{ "from_node": "t", "to_node": "step" }]
    }))
    .expect("graph deserializes")
}

/// The reasons `validate_all` reported, joined for substring assertions.
fn reasons(graph: &WorkflowGraph) -> String {
    validate_all(graph)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn an_agent_node_may_name_its_working_directory() {
    for key in ["cwd", "working_dir"] {
        let graph = graph_with("agent", json!({ "prompt": "go", key: "worktrees/issue-1" }));
        assert!(
            validate_all(&graph).is_empty(),
            "`{key}` is read by an agent node"
        );
    }
}

#[test]
fn a_directory_key_an_agent_node_never_reads_is_refused() {
    // The failure this check exists for: the key is accepted, persisted, and
    // then ignored, so the step runs in the workspace with nothing saying so.
    for key in [
        "workdir",
        "work_dir",
        "working_directory",
        "workspace",
        "directory",
    ] {
        let graph = graph_with("agent", json!({ "prompt": "go", key: "/srv/elsewhere" }));
        let reasons = reasons(&graph);
        assert!(
            reasons.contains(key),
            "`{key}` should be refused: {reasons}"
        );
        assert!(reasons.contains("use `cwd`"), "{reasons}");
    }
}

#[test]
fn a_tool_call_node_is_pointed_at_args_cwd() {
    let graph = graph_with("tool_call", json!({ "slug": "shell.run", "cwd": "build" }));
    let reasons = reasons(&graph);

    assert!(reasons.contains("`args.cwd`"), "{reasons}");
}

#[test]
fn a_sub_workflow_node_may_re_pin_the_child_workspace() {
    let graph = graph_with(
        "sub_workflow",
        json!({ "workflow_id": "child", "workspace": "worktrees/issue-1" }),
    );

    assert!(validate_all(&graph).is_empty());
}

#[test]
fn a_sub_workflow_node_naming_cwd_is_told_the_right_key() {
    let graph = graph_with(
        "sub_workflow",
        json!({ "workflow_id": "child", "cwd": "worktrees/issue-1" }),
    );
    let reasons = reasons(&graph);

    assert!(reasons.contains("use `workspace`"), "{reasons}");
}

#[test]
fn a_trigger_may_pin_the_runs_workspace() {
    let graph: WorkflowGraph = serde_json::from_value(json!({
        "nodes": [{
            "id": "t", "kind": "trigger", "name": "start",
            "config": { "trigger_kind": "manual", "workspace": "/srv/checkout" }
        }],
        "edges": []
    }))
    .expect("graph deserializes");

    assert!(validate_all(&graph).is_empty());
}

#[test]
fn a_node_kind_with_no_working_directory_says_so() {
    let graph = graph_with(
        "transform",
        json!({ "expression": "=item", "cwd": "build" }),
    );
    let reasons = reasons(&graph);

    assert!(reasons.contains("has no working directory"), "{reasons}");
}

#[test]
fn a_shell_node_keeps_reading_cwd() {
    let graph = graph_with("shell", json!({ "source": "pwd", "cwd": "build" }));

    assert!(validate_all(&graph).is_empty());
}
