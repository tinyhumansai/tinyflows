//! Tests for working-directory resolution: what a node may name as the
//! directory it runs in, and what happens to the value when the run is pinned
//! to no workspace at all.
//!
//! Filesystem-only and offline — nothing here runs a graph.

use serde_json::json;

use super::{
    Absolute, resolve_dir_in_workspace, resolve_in_workspace, resolve_node_dir, run_workspace,
};
use crate::caps::WorkdirCheck;

/// A workspace with a `worktrees/issue-1` directory and a file in it.
fn workspace() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(root.path().join("worktrees/issue-1")).expect("mkdir");
    std::fs::write(root.path().join("notes.txt"), "not a directory").expect("write");
    root
}

/// The canonical form of `root`, which is what a resolved path is compared
/// against: a temporary directory is a symlink on some platforms.
fn canonical(root: &tempfile::TempDir) -> std::path::PathBuf {
    root.path().canonicalize().expect("canonicalize")
}

#[test]
fn a_relative_directory_resolves_against_the_workspace() {
    let root = workspace();
    let resolved = resolve_dir_in_workspace(
        root.path(),
        "worktrees/issue-1",
        "config.cwd",
        Absolute::AllowInside,
    )
    .expect("a directory in the workspace resolves");

    assert_eq!(resolved, canonical(&root).join("worktrees/issue-1"));
}

#[test]
fn an_absolute_directory_inside_the_workspace_is_allowed() {
    // The motivating case: an earlier node reports the worktree it created as
    // an absolute path, and the next node binds `cwd` straight to it.
    let root = workspace();
    let inside = canonical(&root).join("worktrees/issue-1");
    let resolved = resolve_dir_in_workspace(
        root.path(),
        &inside.to_string_lossy(),
        "config.cwd",
        Absolute::AllowInside,
    )
    .expect("an absolute path inside the workspace resolves");

    assert_eq!(resolved, inside);
}

#[test]
fn an_absolute_directory_is_refused_where_the_rule_is_stricter() {
    // A script step's `args.script_path` has always been workspace-relative.
    let root = workspace();
    let inside = canonical(&root).join("worktrees/issue-1");
    let error = resolve_in_workspace(
        root.path(),
        &inside.to_string_lossy(),
        "args.cwd",
        Absolute::Refuse,
    )
    .expect_err("Absolute::Refuse takes no absolute path, inside or not");

    assert!(
        error.contains("must be relative to the workspace"),
        "{error}"
    );
}

#[test]
fn a_directory_outside_the_workspace_is_refused() {
    let root = workspace();
    let elsewhere = tempfile::tempdir().expect("tempdir");
    let error = resolve_dir_in_workspace(
        root.path(),
        &elsewhere.path().to_string_lossy(),
        "config.cwd",
        Absolute::AllowInside,
    )
    .expect_err("an absolute path outside the workspace is refused");

    assert!(error.contains("resolves outside the workspace"), "{error}");
}

#[test]
fn a_relative_directory_may_not_traverse_out_of_the_workspace() {
    let root = workspace();
    let error = resolve_dir_in_workspace(
        root.path(),
        "../elsewhere",
        "config.cwd",
        Absolute::AllowInside,
    )
    .expect_err("`..` is refused before the disk is touched");

    assert!(error.contains("must not traverse outside"), "{error}");
}

#[test]
fn a_directory_that_does_not_exist_fails_naming_the_path() {
    let root = workspace();
    let error = resolve_dir_in_workspace(
        root.path(),
        "worktrees/issue-404",
        "config.cwd",
        Absolute::AllowInside,
    )
    .expect_err("a missing directory fails rather than falling back to the workspace");

    assert!(error.contains("worktrees/issue-404"), "{error}");
    assert!(
        error.contains("does not resolve inside the workspace"),
        "{error}"
    );
}

#[test]
fn a_path_that_is_not_a_directory_is_refused() {
    let root = workspace();
    let error = resolve_dir_in_workspace(
        root.path(),
        "notes.txt",
        "config.cwd",
        Absolute::AllowInside,
    )
    .expect_err("a file is not a working directory");

    assert!(error.contains("is not a directory"), "{error}");
}

#[cfg(unix)]
#[test]
fn a_symlink_inside_the_workspace_pointing_out_of_it_is_refused() {
    // The half no amount of string inspection would have caught.
    let root = workspace();
    let elsewhere = tempfile::tempdir().expect("tempdir");
    std::os::unix::fs::symlink(elsewhere.path(), root.path().join("escape")).expect("symlink");

    let error =
        resolve_dir_in_workspace(root.path(), "escape", "config.cwd", Absolute::AllowInside)
            .expect_err("the symlink target is outside the workspace");

    assert!(error.contains("resolves outside the workspace"), "{error}");
}

#[test]
fn the_run_workspace_comes_from_the_run_slice_then_the_trigger() {
    assert_eq!(
        run_workspace(&json!({ "workspace": "/srv/checkout" })),
        Some("/srv/checkout")
    );
    assert_eq!(
        run_workspace(&json!({ "trigger": { "workspace": "/srv/from-trigger" } })),
        Some("/srv/from-trigger"),
        "a host may pin a workspace per run without editing the graph"
    );
    assert_eq!(
        run_workspace(&json!({
            "workspace": "/srv/seeded",
            "trigger": { "workspace": "/srv/from-trigger" }
        })),
        Some("/srv/seeded"),
        "the seeded run workspace wins"
    );
    assert_eq!(run_workspace(&json!({ "workspace": "  " })), None);
    assert_eq!(run_workspace(&json!({})), None);
}

#[tokio::test]
async fn a_run_with_no_workspace_passes_the_directory_through() {
    // A harness whose agents run in a remote sandbox names directories this
    // process has never heard of; checking them locally would fail every one.
    let resolved = resolve_node_dir(
        None,
        &json!({}),
        "/srv/checkout",
        "config.cwd",
        "agent node a",
    )
    .await
    .expect("no workspace, no resolution");

    assert_eq!(resolved, "/srv/checkout");
}

#[tokio::test]
async fn a_resolved_directory_is_reported_with_the_node_surface() {
    let root = workspace();
    let run = json!({ "workspace": root.path().to_string_lossy() });
    let error = resolve_node_dir(None, &run, "nope", "config.cwd", "agent node prepare")
        .await
        .expect_err("a missing directory fails the step");

    assert!(error.to_string().contains("agent node prepare:"), "{error}");
}

/// An [`AgentRunner`] whose agents run on a filesystem this process cannot see:
/// it answers for its own workspace and never touches the local disk.
struct RemoteHarness {
    answer: WorkdirCheck,
}

#[async_trait::async_trait]
impl crate::caps::AgentRunner for RemoteHarness {
    async fn run_agent(
        &self,
        _agent_ref: &str,
        _request: serde_json::Value,
        _conn: Option<&str>,
    ) -> crate::error::Result<serde_json::Value> {
        Ok(json!({}))
    }

    async fn resolve_workdir(&self, _workspace: &str, _declared: &str) -> WorkdirCheck {
        self.answer.clone()
    }
}

fn harness(answer: WorkdirCheck) -> std::sync::Arc<dyn crate::caps::AgentRunner> {
    std::sync::Arc::new(RemoteHarness { answer })
}

#[tokio::test]
async fn a_harness_that_owns_the_workspace_answers_for_it() {
    // The workspace and the directory are both remote: nothing here exists on
    // this process's disk, and the local check would have refused both.
    let runner = harness(WorkdirCheck::Resolved("/remote/ws/worktrees/1".to_string()));
    let run = json!({ "workspace": "/remote/ws" });

    let resolved = resolve_node_dir(
        Some(&runner),
        &run,
        "worktrees/1",
        "config.cwd",
        "agent node code",
    )
    .await
    .expect("the harness resolved it");

    assert_eq!(resolved, "/remote/ws/worktrees/1");
}

#[tokio::test]
async fn a_harness_refusal_fails_the_step_with_its_reason() {
    let runner = harness(WorkdirCheck::Refused("is not in the sandbox".to_string()));
    let run = json!({ "workspace": "/remote/ws" });

    let error = resolve_node_dir(
        Some(&runner),
        &run,
        "worktrees/1",
        "config.cwd",
        "agent node code",
    )
    .await
    .expect_err("a refusal fails the step");

    let error = error.to_string();
    assert!(error.contains("agent node code:"), "{error}");
    assert!(error.contains("is not in the sandbox"), "{error}");
    assert!(error.contains("worktrees/1"), "{error}");
}

#[tokio::test]
async fn an_unmanaged_answer_falls_back_to_the_local_filesystem() {
    // The default for every existing host: the engine checks its own disk,
    // exactly as it did before the capability existed.
    let runner = harness(WorkdirCheck::Unmanaged);
    let root = workspace();
    let run = json!({ "workspace": root.path().to_string_lossy() });

    let resolved = resolve_node_dir(
        Some(&runner),
        &run,
        "worktrees/issue-1",
        "config.cwd",
        "agent node code",
    )
    .await
    .expect("the local filesystem answers");

    assert!(resolved.ends_with("worktrees/issue-1"), "{resolved}");
}

#[tokio::test]
async fn the_shape_check_runs_before_the_harness_is_consulted() {
    // A host cannot be asked to bless a `..` escape: the syntactic half is the
    // engine's, on every filesystem.
    let runner = harness(WorkdirCheck::Resolved("/anywhere".to_string()));
    let run = json!({ "workspace": "/remote/ws" });

    let error = resolve_node_dir(
        Some(&runner),
        &run,
        "../../etc",
        "config.cwd",
        "agent node code",
    )
    .await
    .expect_err("traversal is refused before the harness sees it");

    assert!(
        error.to_string().contains("must not traverse outside"),
        "{error}"
    );
}
