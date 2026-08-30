//! A [`ShellRunner`] over the out-of-process script runner.
//!
//! [`crate::caps::shell`] states the contract a `shell` node needs and stops
//! there, because the engine must not decide which paths are reachable or which
//! environment a script inherits. This module is the other half: the ordinary
//! answer for a host that runs scripts as child processes of itself, so such a
//! host wires a field rather than reimplementing process plumbing, path
//! validation, and the stdin/stdout convention.
//!
//! Two decisions are the host's and stay parameters here:
//!
//! - **Which files a step may reach**, via the [`ScriptPolicy`] this is built
//!   with. An author's `script` path and `cwd` are untrusted strings; they are
//!   resolved inside the configured workspace or refused.
//! - **How long a script may run**, via the timeout. Unbounded is not an option
//!   — a `shell` node that never returns holds its run open forever.
//!
//! What stays *not* a decision is containment: this is not a sandbox, and a
//! script it runs holds the privileges of the process that started it. A host
//! that needs isolation implements [`ShellRunner`] over its own sandbox instead.

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;

use crate::caps::shell::{ShellInterpreter, ShellOutcome, ShellRequest, ShellRunner, ShellScript};
use crate::error::Result;

use super::script::{Interpreter, ScriptLanguage, ScriptRequest, ScriptSource, run_script_capture};
use super::script_policy::ScriptPolicy;

/// A [`ShellRunner`] that spawns each script as a child process.
pub struct ProcessShellRunner {
    /// Where a script may be read from, and where it may run.
    policy: ScriptPolicy,
    /// How long one script may run before it is abandoned.
    timeout: Duration,
}

impl ProcessShellRunner {
    /// A runner bounded by `policy` and `timeout`.
    #[must_use]
    pub fn new(policy: ScriptPolicy, timeout: Duration) -> Self {
        Self { policy, timeout }
    }

    /// The working directory to run in: the author's, resolved in the
    /// workspace, or the workspace itself when they named none.
    ///
    /// Falling back to the workspace rather than to a temporary directory is
    /// deliberate. A `shell` node exists to touch the operator's project; a step
    /// that omitted `cwd` meant "the usual place", and a scratch directory would
    /// satisfy that request by running the script somewhere nothing it cares
    /// about exists.
    fn working_dir(&self, requested: Option<&str>) -> Result<Option<PathBuf>> {
        match requested.map(str::trim).filter(|cwd| !cwd.is_empty()) {
            Some(cwd) => self.policy.resolve_cwd(cwd).map(Some),
            None => Ok(self.policy.workspace().map(Path::to_path_buf)),
        }
    }
}

#[async_trait]
impl ShellRunner for ProcessShellRunner {
    async fn run(&self, request: ShellRequest) -> Result<ShellOutcome> {
        let ShellRequest {
            interpreter,
            script,
            cwd,
            env,
            input,
        } = request;

        // The node's chosen shell is honoured exactly: `sh` and `bash` are the
        // two the engine advertises, and an author who wrote one of them said
        // which, so neither follows a host-configured default.
        let interpreter = Interpreter::validated(
            match interpreter {
                ShellInterpreter::Sh => "sh",
                ShellInterpreter::Bash => "bash",
            },
            &[],
        )?;

        // Resolved before anything is spawned, so a refused path fails the node
        // without a process ever existing.
        let staged = match &script {
            ShellScript::Path(raw) => Some(self.policy.resolve_script(raw)?),
            ShellScript::Inline(_) => None,
        };
        let working_dir = self.working_dir(cwd.as_deref())?;

        let source = match (&script, &staged) {
            (ShellScript::Inline(source), _) => ScriptSource::Inline(source.as_str()),
            (ShellScript::Path(_), Some(path)) => ScriptSource::File(path.as_path()),
            // Unreachable: `staged` is `Some` for exactly the `Path` case above.
            (ShellScript::Path(raw), None) => ScriptSource::Inline(raw.as_str()),
        };

        let completion = run_script_capture(ScriptRequest {
            language: ScriptLanguage::Shell,
            interpreter: Some(&interpreter),
            source,
            input: &input,
            timeout: self.timeout,
            cwd: working_dir.as_deref(),
            env: &env,
        })
        .await?;

        // A non-zero exit is reported, not raised: the `shell` node is what
        // turns a failing script into a failing step, and collapsing the two
        // here would make a host error and a script error indistinguishable.
        Ok(ShellOutcome {
            exit_code: completion.exit_code,
            stdout: completion.stdout,
            stderr: completion.stderr,
        })
    }
}

#[cfg(test)]
#[path = "shell_tests.rs"]
mod tests;
