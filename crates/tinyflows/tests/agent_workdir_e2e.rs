#![cfg(feature = "mock")]
//! End-to-end tests for **where an agent step runs**.
//!
//! A run is pinned to one workspace, and until now every `agent` node's harness
//! booted in it: a `cwd` on the node was accepted, persisted into the stored
//! graph, and silently ignored. These drive the real engine to pin the opposite
//! — that the directory an author names is the directory the harness is told
//! about, that it may be a value an earlier node produced, and that a bad one
//! fails the step instead of quietly falling back to the workspace.

use serde_json::{Value, json};
use tinyflows::caps::mock::{MockAgentHarness, mock_capabilities_with_agent};
use tinyflows::compiler::compile;
use tinyflows::engine::{RunInput, run};
use tinyflows::model::WorkflowGraph;

/// A workspace holding the worktree an issue workflow would have prepared.
fn workspace() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(root.path().join("worktrees/issue-1")).expect("mkdir");
    root
}

/// The canonical workspace path, which is what a resolved `cwd` is compared
/// against: a temporary directory is a symlink on some platforms.
fn canonical(root: &tempfile::TempDir) -> String {
    root.path()
        .canonicalize()
        .expect("canonicalize")
        .to_string_lossy()
        .into_owned()
}

/// A graph whose run is pinned to `workspace`, preparing a worktree path in one
/// node and running an agent with `cwd` set to `cwd_config`.
fn graph_json(workspace: &str, cwd_config: Value) -> Value {
    json!({
        "name": "implement",
        "nodes": [
            {
                "id": "t", "kind": "trigger", "name": "start",
                "config": { "trigger_kind": "manual", "workspace": workspace }
            },
            {
                "id": "prepare", "kind": "transform", "name": "Prepare worktree",
                "config": { "set": { "worktree": "worktrees/issue-1" } }
            },
            {
                "id": "code", "kind": "agent", "name": "Implement",
                "config": { "agent_ref": "coder", "prompt": "Implement it.", "cwd": cwd_config }
            }
        ],
        "edges": [
            { "from_node": "t", "to_node": "prepare" },
            { "from_node": "prepare", "to_node": "code" }
        ]
    })
}

fn parse(graph: Value) -> WorkflowGraph {
    serde_json::from_value(graph).expect("graph deserializes")
}

/// Runs `graph` against a harness that echoes the request it was handed.
async fn run_graph(graph: &WorkflowGraph) -> Result<Value, String> {
    let compiled = compile(graph).expect("compile");
    let caps = mock_capabilities_with_agent(MockAgentHarness::new());
    run(&compiled, RunInput::new(Value::Null), &caps)
        .await
        .map(|outcome| outcome.output["nodes"]["code"]["items"][0]["json"]["json"].clone())
        .map_err(|e| e.to_string())
}

#[tokio::test]
async fn a_relative_cwd_resolves_against_the_run_workspace() {
    let root = workspace();
    let graph = parse(graph_json(&canonical(&root), json!("worktrees/issue-1")));

    let request = run_graph(&graph).await.expect("run");

    assert_eq!(
        request["working_dir"],
        json!(format!("{}/worktrees/issue-1", canonical(&root))),
        "the harness is told the resolved directory, not the raw config string"
    );
}

#[tokio::test]
async fn an_absolute_cwd_inside_the_workspace_is_honored() {
    let root = workspace();
    let inside = format!("{}/worktrees/issue-1", canonical(&root));
    let graph = parse(graph_json(&canonical(&root), json!(inside)));

    let request = run_graph(&graph).await.expect("run");

    assert_eq!(request["working_dir"], json!(inside));
}

#[tokio::test]
async fn a_cwd_bound_to_an_earlier_nodes_output_resolves() {
    // The whole point of the feature: the directory is one an earlier node
    // produced, so it cannot be written literally in the graph.
    let root = workspace();
    let graph = parse(graph_json(
        &canonical(&root),
        json!("=nodes.prepare.item.worktree"),
    ));

    let request = run_graph(&graph).await.expect("run");

    assert_eq!(
        request["working_dir"],
        json!(format!("{}/worktrees/issue-1", canonical(&root)))
    );
}

#[tokio::test]
async fn a_cwd_outside_the_workspace_is_refused() {
    let root = workspace();
    let elsewhere = tempfile::tempdir().expect("tempdir");
    let graph = parse(graph_json(
        &canonical(&root),
        json!(elsewhere.path().to_string_lossy()),
    ));

    let error = run_graph(&graph).await.expect_err("the step must fail");

    assert!(error.contains("resolves outside the workspace"), "{error}");
    assert!(error.contains("agent node code"), "{error}");
}

#[tokio::test]
async fn a_cwd_that_does_not_exist_fails_naming_the_path() {
    let root = workspace();
    let graph = parse(graph_json(&canonical(&root), json!("worktrees/issue-404")));

    let error = run_graph(&graph).await.expect_err("the step must fail");

    assert!(
        error.contains("worktrees/issue-404"),
        "the message names the directory: {error}"
    );
    assert!(
        !error.contains("worktrees/issue-1"),
        "and it does not quietly fall back to the workspace: {error}"
    );
}

#[tokio::test]
async fn a_cwd_expression_that_resolves_to_null_fails_the_step() {
    // The upstream node did not publish the key the `cwd` expression reads, so
    // the resolved config carries `null`. That must fail here rather than read
    // as "no `cwd` declared" and let the harness pick its own directory.
    let root = workspace();
    let graph = parse(graph_json(
        &canonical(&root),
        json!("=nodes.prepare.item.missing_key"),
    ));

    let error = run_graph(&graph).await.expect_err("the step must fail");

    assert!(error.contains("resolved to null"), "{error}");
    assert!(error.contains("agent node code"), "{error}");
}

#[tokio::test]
async fn a_null_cwd_does_not_fall_back_to_working_dir() {
    // Both spellings present, `cwd` resolving to null: the older `working_dir`
    // must not quietly win. Picking it up would run the step in a directory the
    // author's `cwd` expression was meant to override.
    let root = workspace();
    let mut graph = graph_json(&canonical(&root), json!("=nodes.prepare.item.missing_key"));
    graph["nodes"][2]["config"]["working_dir"] = json!("worktrees/issue-1");
    let graph = parse(graph);

    let error = run_graph(&graph).await.expect_err("the step must fail");

    assert!(error.contains("resolved to null"), "{error}");
    assert!(
        !error.contains("worktrees/issue-1"),
        "the fallback is never reached: {error}"
    );
}

#[tokio::test]
async fn a_run_with_no_workspace_passes_the_directory_through_unchanged() {
    // Back-compat: a harness whose agents run in a sandbox names directories
    // this process has never heard of.
    let graph = parse(json!({
        "nodes": [
            { "id": "t", "kind": "trigger", "name": "start", "config": { "trigger_kind": "manual" } },
            {
                "id": "code", "kind": "agent", "name": "Implement",
                "config": { "agent_ref": "coder", "prompt": "go", "working_dir": "/srv/checkout" }
            }
        ],
        "edges": [{ "from_node": "t", "to_node": "code" }]
    }));

    let request = run_graph(&graph).await.expect("run");

    assert_eq!(request["working_dir"], json!("/srv/checkout"));
}

#[tokio::test]
async fn a_sub_workflow_may_run_its_child_in_another_directory() {
    let root = workspace();
    let child = json!({
        "name": "child",
        "nodes": [
            { "id": "ct", "kind": "trigger", "name": "start", "config": { "trigger_kind": "manual" } },
            {
                "id": "code", "kind": "agent", "name": "Implement",
                "config": { "agent_ref": "coder", "prompt": "go", "cwd": "." }
            }
        ],
        "edges": [{ "from_node": "ct", "to_node": "code" }]
    });
    let graph = parse(json!({
        "name": "parent",
        "nodes": [
            {
                "id": "t", "kind": "trigger", "name": "start",
                "config": { "trigger_kind": "manual", "workspace": canonical(&root) }
            },
            {
                "id": "child", "kind": "sub_workflow", "name": "Child",
                "config": { "workflow": child, "workspace": "worktrees/issue-1" }
            }
        ],
        "edges": [{ "from_node": "t", "to_node": "child" }]
    }));

    let compiled = compile(&graph).expect("compile");
    let caps = mock_capabilities_with_agent(MockAgentHarness::new());
    let outcome = run(&compiled, RunInput::new(Value::Null), &caps)
        .await
        .expect("run");
    let child_state = &outcome.output["nodes"]["child"]["items"][0]["json"];

    assert_eq!(
        child_state["run"]["workspace"],
        json!(format!("{}/worktrees/issue-1", canonical(&root))),
        "the child run is pinned to the directory the node named"
    );
    assert_eq!(
        child_state["nodes"]["code"]["items"][0]["json"]["json"]["working_dir"],
        json!(format!("{}/worktrees/issue-1", canonical(&root))),
        "and the child's agent resolves `cwd` against it"
    );
}

#[tokio::test]
async fn a_sub_workflow_workspace_may_not_escape_the_parents() {
    let root = workspace();
    let elsewhere = tempfile::tempdir().expect("tempdir");
    let child = json!({
        "nodes": [{ "id": "ct", "kind": "trigger", "name": "start", "config": { "trigger_kind": "manual" } }],
        "edges": []
    });
    let graph = parse(json!({
        "nodes": [
            {
                "id": "t", "kind": "trigger", "name": "start",
                "config": { "trigger_kind": "manual", "workspace": canonical(&root) }
            },
            {
                "id": "child", "kind": "sub_workflow", "name": "Child",
                "config": { "workflow": child, "workspace": elsewhere.path().to_string_lossy() }
            }
        ],
        "edges": [{ "from_node": "t", "to_node": "child" }]
    }));

    let compiled = compile(&graph).expect("compile");
    let caps = mock_capabilities_with_agent(MockAgentHarness::new());
    let error = run(&compiled, RunInput::new(Value::Null), &caps)
        .await
        .expect_err("the child must not run outside the parent's workspace")
        .to_string();

    assert!(error.contains("resolves outside the workspace"), "{error}");
    assert!(error.contains("sub_workflow node child"), "{error}");
}
