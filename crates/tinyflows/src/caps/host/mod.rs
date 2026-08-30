//! Ready-made capability implementations for a host that runs on an ordinary
//! machine.
//!
//! [`crate::caps`] states what the engine needs and deliberately implements
//! none of it, so the crate never hard-codes a vendor. That rule is about
//! *policy* — which model, which tools, which network, which sandbox — and it
//! stands. But several capabilities have no vendor in them at all: writing a
//! script to a temporary file and reading its stdout, keying JSON documents onto
//! disk, refusing a URL that resolves into a private range. Every host that runs
//! outside a sandbox needs those, and each one that wrote them itself wrote the
//! same subtle parts again: the stdin/stdout deadlock, the DNS-rebinding window
//! between vetting a name and connecting to it, the traversal check that has to
//! canonicalize because a symlink inside the workspace can still point out of
//! it.
//!
//! So they live here, behind the `host-caps` feature, as *implementations a host
//! may choose* rather than behaviour the engine assumes. Nothing in the engine
//! reaches into this module; a host wires what it wants into
//! [`Capabilities`](crate::caps::Capabilities) and supplies its own for the
//! rest. A host with a real sandbox should implement [`CodeRunner`] and
//! [`ShellRunner`] over that instead — these run a script with the privileges of
//! the process that started it, and say so.
//!
//! # What is here
//!
//! - [`script`] — the out-of-process script runner and its calling convention.
//! - [`script_policy`] — which files a script step may read and run in.
//! - [`code`] — [`CodeRunner`] for `code` nodes: refusing, or executing.
//! - [`shell`] — [`ShellRunner`] for `shell` nodes, over the same runner.
//! - [`state`] — [`StateStore`](crate::caps::StateStore) over files.
//! - [`http`] — [`HttpClient`](crate::caps::HttpClient) behind a host allowlist.
//! - [`mocks`] — schema-aware stand-ins for validating a graph by simulation.
//!
//! [`CodeRunner`]: crate::caps::CodeRunner
//! [`ShellRunner`]: crate::caps::ShellRunner

pub mod code;
pub mod http;
pub mod mocks;
pub mod script;
pub mod script_policy;
pub mod shell;
pub mod state;

pub use self::code::{DeniedCodeRunner, ProcessCodeRunner};
pub use self::http::{
    AllowlistHttpClient, HTTP_CRED_PREFIX, HostAllowlist, HttpCredential, http_cred_name,
    inject_credential, is_private_addr, is_private_host, redacted_summary, vet_resolution,
};
pub use self::mocks::{SchemaAwareMockAgentRunner, SchemaAwareMockLlm, sample_for_schema};
pub use self::script::{
    DEFAULT_SHELL, INPUT_ENV, Interpreter, ScriptCompletion, ScriptLanguage, ScriptOutput,
    ScriptRequest, ScriptSource, USER_SHELL, run_script, run_script_capture,
};
pub use self::script_policy::{ScriptPolicy, is_valid_env_name, read_env};
pub use self::shell::ProcessShellRunner;
pub use self::state::FileStateStore;
