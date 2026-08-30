//! Tests for the process-backed `shell` capability.
//!
//! These spawn a real `sh`, which is the point: the contract this implements is
//! about exit codes, working directories, and refused paths, and none of those
//! can be checked against a stand-in. Unix-only, because [`run_script_capture`]
//! refuses shell scripts on Windows outright.
//!
//! [`run_script_capture`]: super::super::script::run_script_capture

#![cfg(unix)]

use std::collections::BTreeMap;

use serde_json::json;

use super::*;

/// The timeout every case here runs under. Generous enough that a loaded
/// machine does not fail a case, short enough that a hung script does not hang
/// the suite.
const TIMEOUT: Duration = Duration::from_secs(30);

/// A runner rooted at `workspace`.
fn runner(workspace: &Path) -> ProcessShellRunner {
    ProcessShellRunner::new(ScriptPolicy::new(&workspace.to_string_lossy()), TIMEOUT)
}

/// The plain request shape: an inline script, no declared environment, no
/// working directory of its own.
fn inline(source: &str) -> ShellRequest {
    ShellRequest {
        interpreter: ShellInterpreter::Sh,
        script: ShellScript::Inline(source.to_string()),
        cwd: None,
        env: BTreeMap::new(),
        input: json!({}),
    }
}

#[tokio::test]
async fn an_inline_script_reports_its_output() {
    let workspace = tempfile::tempdir().expect("workspace");

    let outcome = runner(workspace.path())
        .run(inline("echo hello"))
        .await
        .expect("runs");

    assert_eq!(outcome.exit_code, 0);
    assert_eq!(outcome.stdout, "hello");
    assert!(outcome.is_success());
}

#[tokio::test]
async fn a_failing_script_reports_its_code_rather_than_erroring() {
    let workspace = tempfile::tempdir().expect("workspace");

    // The whole reason `run_script_capture` exists: a non-zero exit is the
    // node's answer, not this layer's failure.
    let outcome = runner(workspace.path())
        .run(inline("echo trouble >&2; exit 3"))
        .await
        .expect("runs");

    assert_eq!(outcome.exit_code, 3);
    assert_eq!(outcome.stderr, "trouble");
    assert!(!outcome.is_success());
}

#[tokio::test]
async fn the_input_reaches_the_script_by_path() {
    let workspace = tempfile::tempdir().expect("workspace");
    let mut request = inline("cat \"$TINYFLOWS_INPUT\"");
    request.input = json!({"answer": 42});

    let outcome = runner(workspace.path()).run(request).await.expect("runs");

    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&outcome.stdout).expect("json"),
        json!({"answer": 42})
    );
}

#[tokio::test]
async fn a_declared_variable_reaches_the_script() {
    let workspace = tempfile::tempdir().expect("workspace");
    let mut request = inline("printf %s \"$GREETING\"");
    request.env = BTreeMap::from([("GREETING".to_string(), "ahoy".to_string())]);

    let outcome = runner(workspace.path()).run(request).await.expect("runs");

    assert_eq!(outcome.stdout, "ahoy");
}

#[tokio::test]
async fn a_script_without_a_cwd_runs_in_the_workspace() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("marker.txt"), "found me").expect("marker");

    // Not a temporary directory: a `shell` node that named no `cwd` still means
    // the operator's project, and a scratch directory would hide that.
    let outcome = runner(workspace.path())
        .run(inline("cat marker.txt"))
        .await
        .expect("runs");

    assert_eq!(outcome.stdout, "found me");
}

#[tokio::test]
async fn a_script_file_in_the_workspace_runs() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("run.sh"), "echo from a file").expect("script");

    let mut request = inline("");
    request.script = ShellScript::Path("run.sh".to_string());

    let outcome = runner(workspace.path()).run(request).await.expect("runs");

    assert_eq!(outcome.stdout, "from a file");
}

#[tokio::test]
async fn a_script_path_outside_the_workspace_is_refused() {
    let workspace = tempfile::tempdir().expect("workspace");
    let mut request = inline("");
    request.script = ShellScript::Path("../escape.sh".to_string());

    let err = request_error(workspace.path(), request).await;

    assert!(err.contains("traverse outside the workspace"), "{err}");
}

#[tokio::test]
async fn a_cwd_outside_the_workspace_is_refused() {
    let workspace = tempfile::tempdir().expect("workspace");
    let mut request = inline("pwd");
    request.cwd = Some("/etc".to_string());

    let err = request_error(workspace.path(), request).await;

    assert!(err.contains("must be relative to the workspace"), "{err}");
}

#[tokio::test]
async fn a_refused_path_never_spawns_anything() {
    let workspace = tempfile::tempdir().expect("workspace");
    // The script would create the file if it ran; a refusal must happen before
    // that, so its absence is the assertion.
    let witness = workspace.path().join("ran.txt");
    let mut request = inline(&format!("touch {}", witness.display()));
    request.cwd = Some("../elsewhere".to_string());

    let _ = request_error(workspace.path(), request).await;

    assert!(!witness.exists(), "the script ran despite a refused cwd");
}

/// Run `request` expecting a refusal, returning the message.
async fn request_error(workspace: &Path, request: ShellRequest) -> String {
    runner(workspace)
        .run(request)
        .await
        .expect_err("refused")
        .to_string()
}
