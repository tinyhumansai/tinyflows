//! The `shell` node's host capability: running a shell script out of process.
//!
//! Shell execution is deliberately *not* folded into [`CodeRunner`] — a shell
//! step needs a working directory, an environment, and the process's exit
//! status and streams, none of which the code capability's
//! `(language, source, input) -> Value` shape can carry.
//!
//! The engine stays host-agnostic here in the strongest sense: it never touches
//! the filesystem or spawns anything. It parses a node's config into a
//! [`ShellRequest`] and hands it to the host, which decides which paths are
//! reachable, which environment a script inherits, and whether shell steps are
//! permitted at all. Path and working-directory strings in a request are
//! **untrusted authoring input**; a host must validate them before use.
//!
//! [`CodeRunner`]: crate::caps::CodeRunner

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde_json::Value;

use crate::error::Result;

/// The shell runtime that executes a `shell` node's script.
///
/// Closed on purpose: an author cannot name an arbitrary interpreter binary,
/// because that would turn `config.interpreter` into a second way to choose
/// what program the host runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ShellInterpreter {
    /// POSIX `sh` — the default, and the portable choice.
    #[default]
    Sh,
    /// GNU Bash, for scripts that need `pipefail`, arrays, or `[[ … ]]`.
    Bash,
}

impl ShellInterpreter {
    /// The wire name an author writes in `config.interpreter`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sh => "sh",
            Self::Bash => "bash",
        }
    }

    /// Parses a wire name, returning `None` for anything not advertised.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "sh" => Some(Self::Sh),
            "bash" => Some(Self::Bash),
            _ => None,
        }
    }
}

/// Where the script to run comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellScript {
    /// A script written inline in the workflow document, so a reviewer reading
    /// the workflow sees exactly what will execute.
    Inline(String),
    /// A path to an external script file, as written by the workflow author.
    ///
    /// This is untrusted configuration: the engine passes the string through
    /// verbatim and the host is responsible for resolving it against whatever
    /// root it permits, and for rejecting traversal outside that root.
    Path(String),
}

/// One `shell` node's execution request.
#[derive(Debug, Clone)]
pub struct ShellRequest {
    /// The shell runtime to execute with.
    pub interpreter: ShellInterpreter,
    /// The script itself, inline or by path.
    pub script: ShellScript,
    /// The working directory the author asked for, if any.
    ///
    /// Untrusted configuration, exactly like [`ShellScript::Path`]. `None`
    /// leaves the choice to the host.
    pub cwd: Option<String>,
    /// Environment variables the author declared, sorted by name so a request
    /// is reproducible.
    ///
    /// Whether these are added to an inherited environment or are the entire
    /// environment is the host's decision, not the engine's.
    pub env: BTreeMap<String, String>,
    /// The node's input items, serialized as JSON for the script to read.
    pub input: Value,
}

/// What running a script produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellOutcome {
    /// The process exit status. Hosts report a negative value when a process
    /// was terminated by a signal rather than exiting normally.
    pub exit_code: i32,
    /// Everything the script wrote to standard output, lossily decoded as
    /// UTF-8.
    pub stdout: String,
    /// Everything the script wrote to standard error, lossily decoded as
    /// UTF-8.
    pub stderr: String,
}

impl ShellOutcome {
    /// Whether the process exited successfully.
    #[must_use]
    pub fn is_success(&self) -> bool {
        self.exit_code == 0
    }
}

/// Runs a shell script for a `shell` node.
///
/// A host that does not want workflows executing shell scripts leaves
/// [`Capabilities::shell`](crate::caps::Capabilities::shell) as `None`, and the
/// `shell` node fails with a capability error naming the missing capability.
#[async_trait]
pub trait ShellRunner: Send + Sync {
    /// Executes `request`, returning the process outcome.
    ///
    /// A non-zero exit is *not* an error here — it is reported through
    /// [`ShellOutcome::exit_code`] so the host's own failures (an unreachable
    /// path, a rejected environment name, a timeout) stay distinguishable from
    /// a script that ran and failed. The `shell` node is what turns a non-zero
    /// exit into a failed step.
    ///
    /// # Errors
    /// Returns an [`EngineError::Capability`](crate::error::EngineError::Capability)
    /// when the host refuses the request or cannot run it at all.
    async fn run(&self, request: ShellRequest) -> Result<ShellOutcome>;
}
