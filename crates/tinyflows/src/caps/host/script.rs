//! Running a script out-of-process, for `code` and `shell` nodes.
//!
//! One executor behind both, so a workflow author learns one calling convention
//! rather than two. What it does is deliberately small: write the source to a
//! temporary file, run it, read stdout.
//!
//! # The calling convention
//!
//! A script gets its input **on stdin, as JSON**, and returns its result **on
//! stdout**. Stdout that parses as JSON becomes structured output; anything else
//! becomes a string, so a script that prints one line is still usable.
//!
//! Stdin rather than an argument because it is the one channel every language
//! reads the same way — `JSON.parse(require('fs').readFileSync(0,'utf8'))`,
//! `json.load(sys.stdin)`, `cat`. The input is *also* written to a file whose
//! path is `argv[1]` and [`INPUT_ENV`], because a large payload through a pipe
//! is awkward in shell and a path is not.
//!
//! # What this is not
//!
//! Not a sandbox. The child inherits this process's environment and privileges,
//! and the only boundary is a temporary working directory, which is not one.
//! What *is* checked, at the boundary above this one, is where a script may come
//! from and where it may run: see [`super::script_policy`]. A host decides
//! whether script steps are permitted at all; everything here is about making a
//! trusted script *work correctly*, not about containing an untrusted one.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use crate::error::{EngineError, Result};

/// The environment variable naming the file the script's JSON input was written
/// to. Always set, alongside the same path as `argv[1]`.
pub const INPUT_ENV: &str = "TINYFLOWS_INPUT";

/// A language this host can execute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptLanguage {
    /// Node.js.
    JavaScript,
    /// CPython 3.
    Python,
    /// POSIX shell, run with `bash` unless an [`Interpreter`] says otherwise.
    Shell,
}

impl ScriptLanguage {
    /// The interpreter and the extension its file wants.
    fn program(self) -> (&'static str, &'static str) {
        match self {
            Self::JavaScript => ("node", "js"),
            Self::Python => ("python3", "py"),
            Self::Shell => (DEFAULT_SHELL, "sh"),
        }
    }

    /// The name an author writes in a node's config.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::JavaScript => "javascript",
            Self::Python => "python",
            Self::Shell => "shell",
        }
    }

    /// Parse the name an author wrote, accepting the obvious spellings.
    ///
    /// Forgiving on purpose: an author who writes `bash`, `sh`, `js`, or `py`
    /// meant something unambiguous, and refusing it teaches nothing.
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "javascript" | "js" | "node" | "nodejs" => Some(Self::JavaScript),
            "python" | "python3" | "py" => Some(Self::Python),
            "shell" | "sh" | "bash" => Some(Self::Shell),
            _ => None,
        }
    }

    /// Every spelling an author may write, for an error that teaches.
    pub const NAMES: [&'static str; 3] = ["javascript", "python", "shell"];

    /// Whether `name` picks a specific interpreter rather than naming the
    /// shell family generically.
    ///
    /// `"bash"` and `"sh"` are an author saying *which* shell, and they said it
    /// before `workflows.shell` existed — a step spelled that way keeps
    /// [`DEFAULT_SHELL`] even on a host that configured another shell, so
    /// enabling `shell = "zsh"` cannot silently re-run bash-specific scripts
    /// somewhere else. Only the generic `"shell"` follows the host.
    #[must_use]
    pub fn pins_interpreter(name: &str) -> bool {
        matches!(name.trim().to_ascii_lowercase().as_str(), "bash" | "sh")
    }
}

/// The shell a `shell` script runs under when nothing chooses another.
///
/// Not the operator's login shell: an existing workflow's script was written
/// against *this*, and quietly re-running it under `fish` or `dash` because that
/// is what `$SHELL` happens to say would break it in ways that look like the
/// script's fault. Tracking the login shell is available, but it is opted into —
/// see [`Interpreter::resolve`].
pub const DEFAULT_SHELL: &str = "bash";

/// The configured value that means "whatever the operator's login shell is".
pub const USER_SHELL: &str = "user";

/// The program a script runs under, and the arguments that precede its path.
///
/// Exists so the shell is a decision rather than a constant. The reason an
/// operator reaches for it is almost always the same one: their own functions,
/// aliases, and `PATH` live in `~/.zshrc`, and a script run as
/// `bash <path>` — non-login, non-interactive — sees none of it. Naming `zsh`
/// with `args: ["-l"]` is what puts those back in scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interpreter {
    /// The program to spawn: a bare name resolved on `PATH`, or an absolute
    /// path.
    pub program: String,
    /// Arguments passed before the script path — `["-l"]` for a login shell,
    /// `["-l", "-i"]` to also get aliases, which are not exported.
    pub args: Vec<String>,
}

impl Interpreter {
    /// The default shell, with no leading arguments.
    #[must_use]
    pub fn default_shell() -> Self {
        Self {
            program: DEFAULT_SHELL.to_string(),
            args: Vec::new(),
        }
    }

    /// Choose the interpreter an operator's configuration asks for.
    ///
    /// `configured` is `workflows.shell` (or a node's own `args.shell`):
    ///
    /// - empty — [`DEFAULT_SHELL`], so an unconfigured host is unchanged.
    /// - [`USER_SHELL`] — the login shell `login_shell` reports, falling back to
    ///   [`DEFAULT_SHELL`] when the environment names none. This is the opt-in
    ///   that makes scripts run under whatever the operator actually uses.
    /// - anything else — that program.
    ///
    /// `login_shell` is passed in rather than read from the environment here so
    /// the choice is a pure function of its inputs, testable without mutating
    /// process-global state.
    ///
    /// # Errors
    ///
    /// Refuses a program that is not a bare name or an absolute path, and any
    /// program or argument carrying an interior NUL. A relative path is refused
    /// rather than resolved because what it would resolve *against* is the
    /// script's working directory, which the workflow author chose — so
    /// `workflows.shell = "./sh"` would let a graph decide which binary the
    /// operator's own configuration named.
    pub fn resolve(configured: &str, args: &[String], login_shell: Option<&str>) -> Result<Self> {
        let configured = configured.trim();
        let program = match configured {
            "" => DEFAULT_SHELL,
            USER_SHELL => login_shell
                .map(str::trim)
                .filter(|shell| !shell.is_empty())
                .unwrap_or(DEFAULT_SHELL),
            other => other,
        };
        Self::validated(program, args)
    }

    /// An interpreter from an already-chosen program name, checked.
    ///
    /// # Errors
    ///
    /// As [`resolve`](Self::resolve).
    pub fn validated(program: &str, args: &[String]) -> Result<Self> {
        let program = program.trim();
        if program.is_empty() {
            return Err(refused("the interpreter must not be empty"));
        }
        if program.contains('\0') {
            return Err(refused(format!(
                "the interpreter {program:?} contains a NUL byte, which cannot be passed to a \
                 process"
            )));
        }
        // `is_separator` rather than `MAIN_SEPARATOR`: Windows accepts `/` as
        // well as `\`, so matching only the platform's *preferred* separator
        // would wave `./sh` straight through on the one platform where two
        // spellings exist. It stays exact on unix, where `\` is an ordinary
        // filename character.
        if program.chars().any(std::path::is_separator) && !Path::new(program).is_absolute() {
            return Err(refused(format!(
                "the interpreter {program:?} is a relative path; name a program on PATH (\"zsh\") \
                 or give an absolute path (\"/bin/zsh\")"
            )));
        }
        for arg in args {
            if arg.contains('\0') {
                return Err(refused(format!(
                    "the interpreter argument {arg:?} contains a NUL byte, which cannot be passed \
                     to a process"
                )));
            }
        }
        Ok(Self {
            program: program.to_string(),
            args: args.to_vec(),
        })
    }

    /// The operator's login shell, as the environment reports it.
    ///
    /// `$SHELL` only — no `/etc/passwd` lookup, because the environment is what
    /// a daemon started from a login session actually carries, and a passwd
    /// entry would disagree with it exactly when a user has changed shells
    /// without re-logging in.
    #[must_use]
    pub fn login_shell() -> Option<String> {
        std::env::var("SHELL").ok()
    }
}

/// A refusal about the interpreter, prefixed so a run record says what refused.
fn refused(message: impl AsRef<str>) -> EngineError {
    EngineError::Capability(format!("script: {}", message.as_ref()))
}

impl From<crate::caps::CodeLanguage> for ScriptLanguage {
    fn from(language: crate::caps::CodeLanguage) -> Self {
        match language {
            crate::caps::CodeLanguage::JavaScript => Self::JavaScript,
            crate::caps::CodeLanguage::Python => Self::Python,
        }
    }
}

/// What a script run produced.
#[derive(Debug)]
pub struct ScriptOutput {
    /// Stdout, parsed as JSON when it is JSON.
    pub value: Value,
    /// Stderr, kept whether or not the script succeeded.
    ///
    /// A script that works and warns is the normal case, and discarding what it
    /// said would hide the one thing its author wrote for a reader.
    pub stderr: String,
}

/// What to run: source this host stages, or a file that already exists.
#[derive(Debug, Clone, Copy)]
pub enum ScriptSource<'a> {
    /// Source text, written to a temporary file before it is run.
    Inline(&'a str),
    /// An existing script file.
    ///
    /// Whether a workflow may reach this path is decided *before* it gets here,
    /// by [`super::script_policy`]; nothing below re-checks it.
    File(&'a Path),
}

/// Everything one script run needs.
///
/// A struct rather than six positional arguments, two of which are paths and
/// three of which are optional — an order a caller would eventually get wrong
/// without the compiler noticing.
#[derive(Debug, Clone, Copy)]
pub struct ScriptRequest<'a> {
    /// The language the script is written in. Decides the interpreter, unless
    /// `interpreter` names one, and always decides the staged file's extension.
    pub language: ScriptLanguage,
    /// The interpreter to run under, overriding the one `language` implies.
    ///
    /// `None` keeps the language's own program, which is what a `code` node
    /// wants: its `javascript` and `python` are the contract, not a preference.
    pub interpreter: Option<&'a Interpreter>,
    /// The script itself.
    pub source: ScriptSource<'a>,
    /// The JSON handed to the script on stdin, and written to `argv[1]`.
    pub input: &'a Value,
    /// How long the script may run before it is abandoned.
    pub timeout: Duration,
    /// The directory to run in. `None` uses the temporary directory holding the
    /// staged script — right for a pure computation, wrong for anything that
    /// means to touch the operator's project.
    pub cwd: Option<&'a Path>,
    /// Environment variables layered over the inherited environment.
    pub env: &'a BTreeMap<String, String>,
}

impl<'a> ScriptRequest<'a> {
    /// A request with no working directory and no extra environment — the shape
    /// a pure computation over `input` wants.
    pub fn plain(
        language: ScriptLanguage,
        source: &'a str,
        input: &'a Value,
        timeout: Duration,
        env: &'a BTreeMap<String, String>,
    ) -> Self {
        Self {
            language,
            interpreter: None,
            source: ScriptSource::Inline(source),
            input,
            timeout,
            cwd: None,
            env,
        }
    }

    /// The program this request will spawn — the interpreter it names, or the
    /// one its language implies.
    ///
    /// Public because an error message naming the program is the difference
    /// between "the script failed" and "`node` is not installed".
    #[must_use]
    pub fn program(&self) -> &str {
        match self.interpreter {
            Some(chosen) => chosen.program.as_str(),
            None => self.language.program().0,
        }
    }
}

/// Everything a finished script run produced, including a failing exit status.
///
/// Separate from [`ScriptOutput`] because the two callers disagree about what a
/// non-zero exit *is*. A `code` node's contract is a value, so a script that
/// exits non-zero produced none and the step failed. A `shell` node's contract
/// is a process, so its exit code is part of the answer and the node — not this
/// runner — decides what a failure means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptCompletion {
    /// The process exit status, or `-1` when a signal terminated it.
    pub exit_code: i32,
    /// Everything the script wrote to standard output, lossily decoded.
    pub stdout: String,
    /// Everything the script wrote to standard error, lossily decoded.
    pub stderr: String,
}

mod runner;
pub use runner::{run_script, run_script_capture};

#[cfg(test)]
#[path = "script_tests.rs"]
mod tests;
