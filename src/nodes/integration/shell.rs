//! The `shell` node: shell scripts delegated to the host's shell capability.
//!
//! This module only *parses and validates* a node's config into a
//! [`ShellRequest`]. Everything with an effect — resolving a script path,
//! choosing an environment, spawning a process — belongs to the host behind
//! [`ShellRunner`](crate::caps::ShellRunner).

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde_json::Value;

use crate::caps::{ShellInterpreter, ShellRequest, ShellScript};
use crate::data::Item;
use crate::error::{EngineError, Result};
use crate::nodes::{NodeContext, NodeExecutor, NodeOutput};

/// Executes a shell script via [`crate::caps::ShellRunner`].
#[derive(Debug, Default, Clone)]
pub struct ShellNode;

/// Reads `config.interpreter`, defaulting to `sh`.
fn interpreter_of(config: &Value) -> Result<ShellInterpreter> {
    let Some(named) = config.get("interpreter") else {
        return Ok(ShellInterpreter::default());
    };
    let named = named.as_str().ok_or_else(|| {
        EngineError::Capability("shell: config.interpreter must be a string".to_string())
    })?;
    ShellInterpreter::parse(named).ok_or_else(|| {
        EngineError::Capability(format!(
            "shell: unsupported interpreter '{named}'; expected 'sh' or 'bash'"
        ))
    })
}

/// Reads exactly one of `config.source` (inline) or `config.script_path`.
///
/// Requiring exactly one keeps the node unambiguous: a config carrying both
/// would otherwise silently run whichever the implementation happened to check
/// first, which is precisely the kind of thing a reviewer misses.
fn script_of(config: &Value) -> Result<ShellScript> {
    let source = config.get("source");
    let path = config.get("script_path");

    match (source, path) {
        (Some(_), Some(_)) => Err(EngineError::Capability(
            "shell: set config.source or config.script_path, not both".to_string(),
        )),
        (Some(source), None) => {
            let source = source.as_str().ok_or_else(|| {
                EngineError::Capability("shell: config.source must be a string".to_string())
            })?;
            if source.trim().is_empty() {
                return Err(EngineError::Capability(
                    "shell: config.source must be a non-empty script".to_string(),
                ));
            }
            Ok(ShellScript::Inline(source.to_string()))
        }
        (None, Some(path)) => {
            let path = path.as_str().ok_or_else(|| {
                EngineError::Capability("shell: config.script_path must be a string".to_string())
            })?;
            if path.trim().is_empty() {
                return Err(EngineError::Capability(
                    "shell: config.script_path must be a non-empty path".to_string(),
                ));
            }
            Ok(ShellScript::Path(path.to_string()))
        }
        (None, None) => Err(EngineError::Capability(
            "shell: config.source (inline script) or config.script_path (script file) is required"
                .to_string(),
        )),
    }
}

/// Reads `config.cwd`, rejecting a non-string or blank value.
fn cwd_of(config: &Value) -> Result<Option<String>> {
    let Some(cwd) = config.get("cwd") else {
        return Ok(None);
    };
    let cwd = cwd
        .as_str()
        .ok_or_else(|| EngineError::Capability("shell: config.cwd must be a string".to_string()))?;
    if cwd.trim().is_empty() {
        return Err(EngineError::Capability(
            "shell: config.cwd must be a non-empty path when present".to_string(),
        ));
    }
    Ok(Some(cwd.to_string()))
}

/// Reads `config.env` as a flat string map.
///
/// Only strings are accepted: coercing a number or boolean would make the value
/// a script actually sees depend on JSON formatting rather than on what the
/// author wrote.
fn env_of(config: &Value) -> Result<BTreeMap<String, String>> {
    let Some(env) = config.get("env") else {
        return Ok(BTreeMap::new());
    };
    let env = env.as_object().ok_or_else(|| {
        EngineError::Capability("shell: config.env must be an object of strings".to_string())
    })?;
    env.iter()
        .map(|(name, value)| {
            let value = value.as_str().ok_or_else(|| {
                EngineError::Capability(format!(
                    "shell: config.env.{name} must be a string, not {}",
                    kind_of(value)
                ))
            })?;
            Ok((name.clone(), value.to_string()))
        })
        .collect()
}

/// A short name for a JSON value's type, for error messages.
fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// The last `limit` characters of `text`, marked when anything was dropped.
fn tail(text: &str, limit: usize) -> String {
    let text = text.trim();
    let length = text.chars().count();
    if length <= limit {
        return text.to_string();
    }
    let kept: String = text.chars().skip(length - limit).collect();
    format!("…{kept}")
}

#[async_trait]
impl NodeExecutor for ShellNode {
    async fn execute(&self, ctx: NodeContext<'_>) -> Result<NodeOutput> {
        let config = &ctx.node.config;
        let runner = ctx.caps.shell.as_ref().ok_or_else(|| {
            EngineError::Capability(
                "shell: this host has no shell capability, so `shell` nodes cannot run".to_string(),
            )
        })?;

        let request = ShellRequest {
            interpreter: interpreter_of(config)?,
            script: script_of(config)?,
            cwd: cwd_of(config)?,
            env: env_of(config)?,
            input: serde_json::to_value(ctx.input)
                .map_err(|err| EngineError::Capability(err.to_string()))?,
        };

        let outcome = runner.run(request).await?;
        if !outcome.is_success() {
            // A failed step emits no items, so the streams go into the message:
            // a tail of stderr is what makes the failure diagnosable from a run
            // record alone.
            return Err(EngineError::Capability(format!(
                "shell: script exited with status {}: {}",
                outcome.exit_code,
                tail(&outcome.stderr, STDERR_TAIL_LIMIT)
            )));
        }

        // Structured output when the script printed JSON, alongside the raw
        // streams — a step that pipes text and a step that emits JSON are both
        // ordinary uses, so neither is made to look like the exception.
        let stdout_json: Value = serde_json::from_str(outcome.stdout.trim()).unwrap_or(Value::Null);
        Ok(NodeOutput::main(vec![Item::new(serde_json::json!({
            "exit_code": outcome.exit_code,
            "stdout": outcome.stdout,
            "stderr": outcome.stderr,
            "stdout_json": stdout_json,
        }))]))
    }
}

/// How much of a failed script's standard error is quoted in the step error.
const STDERR_TAIL_LIMIT: usize = 2000;
