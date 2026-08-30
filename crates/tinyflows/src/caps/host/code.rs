//! The `code` node's runner — refusing by default, executing when opted in.
//!
//! A host with a sandbox runs `code` nodes inside it, with its own approval
//! policy deciding whether a call needs consent. A host *without* one — a daemon
//! holding the privileges of the user who started it — executes a workflow
//! author's script with those privileges and no boundary at all.
//!
//! Rather than pretend otherwise, this pair makes the choice explicit.
//! [`DeniedCodeRunner`] refuses and says why; [`ProcessCodeRunner`] runs the
//! script out of process for a host that has opted in. What neither does is
//! refuse and leave the author with nothing: a workflow whose real work is a
//! fifty-line script should be able to say so, and the alternative — an `agent`
//! node whose prompt asks a harness to run it — costs a whole model session for
//! work that takes milliseconds.
//!
//! The execution itself lives in [`super::script`], shared with the `shell`
//! node so an author learns one calling convention.

use crate::caps::{CodeLanguage, CodeRunner};
use crate::error::{EngineError, Result};
use async_trait::async_trait;
use serde_json::Value;

use super::script::{ScriptLanguage, ScriptRequest, run_script};

/// A [`CodeRunner`] that refuses every request, explaining the missing sandbox.
pub struct DeniedCodeRunner;

#[async_trait]
impl CodeRunner for DeniedCodeRunner {
    async fn run(&self, _language: CodeLanguage, _source: &str, _input: Value) -> Result<Value> {
        Err(EngineError::Capability(
            "code nodes are disabled: this host has no sandbox, so workflow code would run with \
             the daemon's full privileges. Enable `workflows.allowCode` only where that is \
             acceptable, or use a `transform` node's expressions instead."
                .to_string(),
        ))
    }
}

/// A [`CodeRunner`] that runs code out-of-process, for hosts that opted in.
///
/// Still not a sandbox — it is the operator's explicit decision to trust the
/// workflow author — but bounded: a temporary working directory and a
/// wall-clock limit.
///
/// The script is given its input on **stdin as JSON**, and also as a file whose
/// path is `argv[1]` and `$TINYFLOWS_INPUT`. It returns its result on **stdout**;
/// stdout that parses as JSON becomes structured output, anything else becomes a
/// string. See [`super::script`] for the whole contract.
pub struct ProcessCodeRunner {
    /// How long one execution may take.
    timeout: std::time::Duration,
}

impl ProcessCodeRunner {
    /// A runner with the given per-execution timeout.
    pub fn new(timeout: std::time::Duration) -> Self {
        Self { timeout }
    }
}

#[async_trait]
impl CodeRunner for ProcessCodeRunner {
    async fn run(&self, language: CodeLanguage, source: &str, input: Value) -> Result<Value> {
        // A `code` node runs in a temporary directory rather than the operator's
        // project: the engine's own contract for the kind is a computation over
        // its input, and a step that means to touch the repo is a
        // `shell` node, which says so in the graph.
        run_script(ScriptRequest::plain(
            ScriptLanguage::from(language),
            source,
            &input,
            self.timeout,
            &std::collections::BTreeMap::new(),
        ))
        .await
        .map(|output| output.value)
    }
}
