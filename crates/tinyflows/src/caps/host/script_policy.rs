//! Validating the untrusted parts of a script step's arguments.
//!
//! A workflow arrives as a file — possibly written by an agent, possibly copied
//! from somewhere. Its `script_path`, `cwd`, and `env` are author-supplied
//! strings that reach the operating system, so they are treated like every other
//! untrusted input in this crate: checked at the boundary, against the
//! operator's own configuration, before anything is spawned.
//!
//! The boundary is the configured workspace. [`super::script`] is deliberately
//! not a sandbox and does not pretend to be one; what this module bounds is
//! narrower and worth having anyway — *which file* a step may execute and
//! *which directory* it may run in. A workflow naming `../../../.ssh/id_rsa`
//! or `/etc/cron.d/x` as its script is answered rather than obeyed.
//!
//! Nothing here restricts what a script does once it runs. It cannot: this host
//! has no sandbox, which is exactly what `workflows.allowCode` says.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::{EngineError, Result};
use crate::workdir::Absolute;

/// Where a script step may read a script from and run.
#[derive(Debug, Clone, Default)]
pub struct ScriptPolicy {
    /// The operator's workspace, and the only directory a `script_path` or a
    /// `cwd` may resolve inside.
    ///
    /// `None` when no workspace is configured, which refuses both: a step can
    /// still run an inline script, in a scratch directory, which needs no
    /// filesystem policy at all.
    workspace: Option<PathBuf>,
}

impl ScriptPolicy {
    /// A policy rooted at `workspace`, or refusing paths entirely when it is
    /// empty or is not a directory.
    pub fn new(workspace: &str) -> Self {
        let candidate = Path::new(workspace);
        Self {
            workspace: (!workspace.trim().is_empty() && candidate.is_dir())
                .then(|| candidate.to_path_buf()),
        }
    }

    /// The workspace itself, when one is configured.
    ///
    /// This is the directory a script runs in unless the step named another.
    pub fn workspace(&self) -> Option<&Path> {
        self.workspace.as_deref()
    }

    /// Resolves an author-supplied script path to a readable file in the
    /// workspace.
    ///
    /// # Errors
    /// Refuses when no workspace is configured, when `raw` is absolute or
    /// traverses upwards, when the resolved path escapes the workspace
    /// (following symlinks), or when it is not an existing regular file.
    pub fn resolve_script(&self, raw: &str) -> Result<PathBuf> {
        let resolved = self.resolve(raw, "script_path")?;
        if !resolved.is_file() {
            return Err(refused(format!(
                "`args.script_path` ('{raw}') is not a file in the workspace"
            )));
        }
        Ok(resolved)
    }

    /// Resolves an author-supplied working directory in the workspace.
    ///
    /// # Errors
    /// As [`Self::resolve_script`], except the result must be a directory.
    pub fn resolve_cwd(&self, raw: &str) -> Result<PathBuf> {
        let resolved = self.resolve(raw, "cwd")?;
        if !resolved.is_dir() {
            return Err(refused(format!(
                "`args.cwd` ('{raw}') is not a directory in the workspace"
            )));
        }
        Ok(resolved)
    }

    /// The shared resolution: reject the shape, then reject the destination.
    ///
    /// Delegated to [`crate::workdir`], which is where that rule now lives so
    /// the `agent` and `sub_workflow` nodes enforce the same one. A script step
    /// keeps the stricter half of it — [`Absolute::Refuse`] — because an
    /// operator's `args.script_path` has always been a workspace-relative name
    /// and nothing should start reading absolute ones.
    fn resolve(&self, raw: &str, field: &str) -> Result<PathBuf> {
        let Some(workspace) = &self.workspace else {
            return Err(refused(format!(
                "`args.{field}` needs a configured workspace to resolve against, and this host \
                 has none; pass `args.script` instead"
            )));
        };

        crate::workdir::resolve_in_workspace(
            workspace,
            raw,
            &format!("args.{field}"),
            Absolute::Refuse,
        )
        .map_err(refused)
    }
}

/// Whether `name` is a usable environment-variable name.
///
/// Stricter than the kernel on purpose: a name carrying `=` or a NUL would be
/// rejected by `execve` anyway, and one carrying a space or a newline is far
/// more likely to be a mistake in the workflow than a deliberate choice.
pub fn is_valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Reads an author-supplied `args.env` object into a string map.
///
/// Only strings are accepted: coercing a number or a boolean would make the
/// value a script actually sees depend on JSON formatting rather than on what
/// the author wrote.
///
/// # Errors
/// Refuses a non-object, a non-string value, a malformed variable name, or a
/// value containing an interior NUL.
pub fn read_env(value: Option<&serde_json::Value>) -> Result<BTreeMap<String, String>> {
    let Some(value) = value else {
        return Ok(BTreeMap::new());
    };
    let object = value.as_object().ok_or_else(|| {
        refused("`args.env` must be an object mapping variable names to string values")
    })?;

    object
        .iter()
        .map(|(name, value)| {
            if !is_valid_env_name(name) {
                return Err(refused(format!(
                    "`args.env` has an invalid variable name '{name}'; expected letters, digits, \
                     and underscores, not starting with a digit"
                )));
            }
            let value = value.as_str().ok_or_else(|| {
                refused(format!(
                    "`args.env.{name}` must be a string, not {}",
                    kind_of(value)
                ))
            })?;
            if value.contains('\0') {
                return Err(refused(format!(
                    "`args.env.{name}` contains a NUL byte, which cannot be passed to a process"
                )));
            }
            Ok((name.clone(), value.to_string()))
        })
        .collect()
}

/// A short name for a JSON value's type, for an error that teaches.
fn kind_of(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
}

/// A refusal, prefixed so a run record says which step surface refused.
fn refused(message: impl AsRef<str>) -> EngineError {
    EngineError::Capability(format!("shell: {}", message.as_ref()))
}

#[cfg(test)]
#[path = "script_policy_tests.rs"]
mod tests;
