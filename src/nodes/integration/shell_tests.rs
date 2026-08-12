//! Tests for `shell`-node config validation and capability delegation.

use serde_json::{Value, json};

use super::ShellNode;
use crate::caps::mock::mock_capabilities;
use crate::caps::{Capabilities, ShellOutcome, ShellRequest, ShellRunner, ShellScript};
use crate::data::Item;
use crate::error::Result;
use crate::model::{Node, NodeKind};
use crate::nodes::{NodeContext, NodeExecutor, NodeOutput};

fn shell_node(config: Value) -> Node {
    Node {
        id: "shell".into(),
        kind: NodeKind::Shell,
        type_version: 1,
        name: "Shell".into(),
        config,
        ports: vec![],
        position: None,
    }
}

async fn execute_with(caps: Capabilities, config: Value) -> Result<NodeOutput> {
    let node = shell_node(config);
    ShellNode
        .execute(NodeContext {
            node: &node,
            input: &[Item::new(json!({
                "seed": 1,
                "script": "printf resolved",
                "cwd": "/srv/resolved",
                "profile": "debug",
            }))],
            run: &Value::Null,
            nodes: &Value::Null,
            caps: &caps,
            token: crate::engine::CancellationToken::new(),
        })
        .await
}

async fn execute(config: Value) -> Result<NodeOutput> {
    execute_with(mock_capabilities(), config).await
}

#[tokio::test]
async fn inline_source_runs_and_surfaces_both_streams() {
    let output = execute(json!({ "source": "printf ok" }))
        .await
        .expect("an inline script is runnable");
    let item = &output.items[0].json;
    assert_eq!(item["exit_code"], 0);
    assert_eq!(item["stdout"], "printf ok");
    assert!(item["stderr"].as_str().expect("stderr").contains("sh cwd="));
    assert!(item["stdout_json"].is_null());
}

#[tokio::test]
async fn json_on_stdout_is_parsed_alongside_the_raw_text() {
    let output = execute(json!({ "source": "{\"built\":true}" }))
        .await
        .expect("a JSON-emitting script is runnable");
    let item = &output.items[0].json;
    assert_eq!(item["stdout_json"], json!({ "built": true }));
    assert_eq!(item["stdout"], "{\"built\":true}");
}

/// A runner that reports the exit code and stderr it was constructed with.
struct FailingShell {
    exit_code: i32,
    stderr: String,
}

#[async_trait::async_trait]
impl ShellRunner for FailingShell {
    async fn run(&self, request: ShellRequest) -> Result<ShellOutcome> {
        assert!(matches!(request.script, ShellScript::Inline(_)));
        Ok(ShellOutcome {
            exit_code: self.exit_code,
            stdout: String::new(),
            stderr: self.stderr.clone(),
        })
    }
}

/// Capability bundle whose shell runner is `runner`.
fn caps_with(runner: impl ShellRunner + 'static) -> Capabilities {
    Capabilities {
        shell: Some(std::sync::Arc::new(runner)),
        ..mock_capabilities()
    }
}

#[tokio::test]
async fn a_non_zero_exit_fails_the_step() {
    let caps = caps_with(FailingShell {
        exit_code: 3,
        stderr: "no such target".to_string(),
    });
    let error = execute_with(caps, json!({ "source": "make all" }))
        .await
        .expect_err("a failing script must fail its step");
    let message = error.to_string();
    assert!(message.contains("exited with status 3"), "{message}");
    assert!(message.contains("no such target"), "{message}");
}

#[tokio::test]
async fn a_script_path_reaches_the_host_verbatim_for_validation() {
    let output = execute(json!({ "script_path": "scripts/build.sh" }))
        .await
        .expect("a path script is runnable");
    // The engine never resolves the path itself; the host sees what was written.
    assert_eq!(output.items[0].json["stdout"], "scripts/build.sh");
}

#[tokio::test]
async fn interpreter_cwd_and_env_are_forwarded() {
    let output = execute(json!({
        "source": "printf ok",
        "interpreter": "bash",
        "cwd": "/srv/build",
        "env": { "PROFILE": "release" },
    }))
    .await
    .expect("a fully configured script is runnable");
    let stderr = output.items[0].json["stderr"]
        .as_str()
        .expect("stderr")
        .to_string();
    assert!(stderr.contains("bash"), "interpreter missing: {stderr}");
    assert!(stderr.contains("cwd=/srv/build"), "cwd missing: {stderr}");
    assert!(stderr.contains("\"PROFILE\":\"release\""), "env: {stderr}");
}

#[tokio::test]
async fn script_cwd_and_env_expressions_are_resolved() {
    let output = execute(json!({
        "source": "=item.script",
        "cwd": "=item.cwd",
        "env": { "PROFILE": "=item.profile" },
        "unused": "=item.missing",
    }))
    .await
    .expect("expression-backed shell config is runnable");

    let item = &output.items[0].json;
    assert_eq!(item["stdout"], "printf resolved");
    let stderr = item["stderr"].as_str().expect("stderr");
    assert!(stderr.contains("cwd=/srv/resolved"), "cwd: {stderr}");
    assert!(stderr.contains("\"PROFILE\":\"debug\""), "env: {stderr}");
    assert_eq!(output.diagnostics.len(), 1);
    assert_eq!(output.diagnostics[0].location, "unused");
    assert_eq!(output.diagnostics[0].expression, "=item.missing");
}

#[tokio::test]
async fn a_missing_script_is_rejected() {
    let error = execute(json!({ "interpreter": "sh" }))
        .await
        .expect_err("a node with no script must not run");
    assert!(error.to_string().contains("is required"));
}

#[tokio::test]
async fn declaring_both_a_script_and_a_path_is_rejected() {
    let error = execute(json!({ "source": "printf ok", "script_path": "a.sh" }))
        .await
        .expect_err("an ambiguous node must not run");
    assert!(error.to_string().contains("not both"));
}

#[tokio::test]
async fn an_empty_script_is_rejected() {
    let error = execute(json!({ "source": "  " }))
        .await
        .expect_err("an empty script must not run");
    assert!(error.to_string().contains("non-empty script"));

    let error = execute(json!({ "script_path": " " }))
        .await
        .expect_err("an empty path must not run");
    assert!(error.to_string().contains("non-empty path"));
}

#[tokio::test]
async fn non_string_config_values_are_rejected() {
    for (config, needle) in [
        (json!({ "source": 1 }), "config.source must be a string"),
        (
            json!({ "script_path": 1 }),
            "config.script_path must be a string",
        ),
        (
            json!({ "source": "x", "interpreter": 1 }),
            "config.interpreter must be a string",
        ),
        (
            json!({ "source": "x", "cwd": 1 }),
            "config.cwd must be a string",
        ),
        (
            json!({ "source": "x", "cwd": " " }),
            "config.cwd must be a non-empty path",
        ),
        (
            json!({ "source": "x", "env": [] }),
            "config.env must be an object",
        ),
        (
            json!({ "source": "x", "env": { "N": 1 } }),
            "config.env.N must be a string, not a number",
        ),
    ] {
        let error = execute(config.clone())
            .await
            .expect_err("malformed config must not run");
        assert!(
            error.to_string().contains(needle),
            "{config} produced {error}"
        );
    }
}

#[tokio::test]
async fn an_unknown_interpreter_is_rejected() {
    let error = execute(json!({ "source": "printf ok", "interpreter": "zsh" }))
        .await
        .expect_err("an unadvertised interpreter must not run");
    assert!(error.to_string().contains("expected 'sh' or 'bash'"));
}

#[tokio::test]
async fn a_host_without_the_capability_says_so() {
    let caps = Capabilities {
        shell: None,
        ..mock_capabilities()
    };
    let error = execute_with(caps, json!({ "source": "printf ok" }))
        .await
        .expect_err("a host without the capability must refuse");
    assert!(error.to_string().contains("no shell capability"));
}

#[tokio::test]
async fn a_long_stderr_is_truncated_to_its_tail() {
    let caps = caps_with(FailingShell {
        exit_code: 1,
        stderr: format!("{}tail-marker", "x".repeat(4000)),
    });
    let error = execute_with(caps, json!({ "source": "noisy" }))
        .await
        .expect_err("a failing script must fail its step");
    let message = error.to_string();
    assert!(message.contains('…'), "not truncated: {message}");
    // The tail is what survives, because the last lines explain the failure.
    assert!(message.ends_with("tail-marker"), "wrong end: {message}");
}
