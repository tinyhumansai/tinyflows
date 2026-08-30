//! Programmable, recording capability doubles.
//!
//! [`crate::caps::mock`] answers every call with the same canned echo. That is
//! enough to prove a graph *executes* and no help at all in proving one is
//! *correct*: there is no way to say "the third call fails", no way to make two
//! tools answer differently, and no way to ask afterwards what a capability was
//! actually handed. Every test that needed any of that wrote its own
//! `AtomicUsize`-counting impl, and there are more than a dozen such
//! one-offs in this repo's own suite.
//!
//! So: rules in, a call log out.
//!
//! ```no_run
//! use std::sync::Arc;
//! use tinyflows::testkit::{MockCaps, Respond};
//! use serde_json::json;
//!
//! // Shared behind an `Arc`, because each node activation is handed its own
//! // bundle over the same rules and the same log.
//! let mocks = Arc::new(MockCaps::new()
//!     .on_tool("slack.send", Respond::value(json!({ "ok": true })))
//!     // First call rate-limits, the retry succeeds — the shape of a flaky
//!     // dependency, without a flaky test.
//!     .on_tool(
//!         "gh.issues.*",
//!         Respond::sequence([
//!             Respond::error("429 rate limited"),
//!             Respond::value(json!({ "number": 7 })),
//!         ]),
//!     ));
//! let capabilities = mocks.capabilities();
//! ```
//!
//! Matching is first-rule-wins in declaration order, so a specific rule written
//! before a general one shadows it. A call matching no rule falls back to the
//! bundle's default behaviour, which is the same echo `caps::mock` gives — a
//! graph under test never fails because a capability was left unprogrammed.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

use crate::caps::{Capabilities, sample_for_schema};
use crate::error::{EngineError, Result};
use crate::model::WorkflowGraph;

#[path = "mocks_log.rs"]
mod log;
pub use log::{CallLog, CallOutcome, CapCall};

#[path = "mocks_double.rs"]
mod double;
use double::Double;

/// Which capability a call went to.
///
/// A plain string rather than an enum on the wire, so a recording written by a
/// build that knows a capability this one does not still parses.
pub mod capability {
    /// [`LlmProvider`](crate::caps::LlmProvider).
    pub const LLM: &str = "llm";
    /// [`ToolInvoker`](crate::caps::ToolInvoker).
    pub const TOOLS: &str = "tools";
    /// [`HttpClient`](crate::caps::HttpClient).
    pub const HTTP: &str = "http";
    /// [`CodeRunner`](crate::caps::CodeRunner).
    pub const CODE: &str = "code";
    /// [`ShellRunner`](crate::caps::ShellRunner).
    pub const SHELL: &str = "shell";
    /// [`AgentRunner`](crate::caps::AgentRunner).
    pub const AGENT: &str = "agent";
    /// [`MemoryProvider`](crate::caps::MemoryProvider).
    pub const MEMORY: &str = "memory";
    /// [`StateStore`](crate::caps::StateStore).
    pub const STATE: &str = "state";
    /// [`ApprovalProvider`](crate::caps::ApprovalProvider).
    pub const APPROVALS: &str = "approvals";
}

/// What a matched rule answers with.
///
/// Construct these through the helpers ([`Respond::value`], [`Respond::error`],
/// …) rather than the variants directly; the variants are public so a host can
/// match on a loaded recording.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Respond {
    /// Return this value.
    Value(Value),
    /// Fail with this message, as an
    /// [`EngineError::Capability`](crate::error::EngineError::Capability).
    Error(String),
    /// Answer the nth matching call with the nth entry.
    ///
    /// Calls past the end repeat the last entry, so a sequence written for the
    /// first two calls does not start failing on the third for a reason the
    /// author never intended. An empty sequence falls through to the default.
    Sequence(Vec<Respond>),
    /// Wait, then answer. For exercising timeouts and genuine concurrency.
    Delay(Duration, Box<Respond>),
    /// Synthesize a value satisfying this JSON Schema.
    ///
    /// The auto-mock: a node declaring an `output_parser.schema` gets something
    /// that shape, so it does not fail validation for a reason unrelated to the
    /// graph. See [`sample_for_schema`].
    Schema(Value),
    /// Echo the request back, exactly as [`crate::caps::mock`] does.
    Echo,
}

impl Respond {
    /// Return `value`.
    #[must_use]
    pub fn value(value: Value) -> Self {
        Self::Value(value)
    }

    /// Fail with `message`.
    #[must_use]
    pub fn error(message: impl Into<String>) -> Self {
        Self::Error(message.into())
    }

    /// Answer successive matching calls with successive entries.
    #[must_use]
    pub fn sequence(entries: impl IntoIterator<Item = Respond>) -> Self {
        Self::Sequence(entries.into_iter().collect())
    }

    /// Wait `delay`, then answer with `then`.
    #[must_use]
    pub fn after(delay: Duration, then: Respond) -> Self {
        Self::Delay(delay, Box::new(then))
    }

    /// Synthesize a value satisfying `schema`.
    #[must_use]
    pub fn schema(schema: Value) -> Self {
        Self::Schema(schema)
    }

    /// Resolve to a concrete answer for the `hit`-th matching call.
    ///
    /// `async` because [`Respond::Delay`] genuinely waits; every other variant
    /// resolves immediately.
    async fn answer(&self, hit: usize, request: &Value) -> Result<Value> {
        match self {
            Self::Value(value) => Ok(value.clone()),
            Self::Error(message) => Err(EngineError::Capability(message.clone())),
            Self::Sequence(entries) => match entries.last() {
                // Past the end, repeat the last entry rather than falling off
                // into a different behaviour the author never wrote.
                Some(last) => {
                    let entry = entries.get(hit).unwrap_or(last);
                    Box::pin(entry.answer(hit, request)).await
                }
                None => Ok(request.clone()),
            },
            Self::Delay(delay, then) => {
                futures_timer::Delay::new(*delay).await;
                Box::pin(then.answer(hit, request)).await
            }
            Self::Schema(schema) => Ok(sample_for_schema(schema)),
            Self::Echo => Ok(request.clone()),
        }
    }
}

/// Which calls a rule applies to.
#[derive(Debug, Clone)]
struct Matcher {
    capability: String,
    /// A glob over the call's target. `*` matches any run of characters.
    target: String,
    /// When set, the rule only applies to calls made by this node.
    node_id: Option<String>,
}

impl Matcher {
    fn matches(&self, capability: &str, target: &str, node_id: Option<&str>) -> bool {
        self.capability == capability
            && glob_matches(&self.target, target)
            && match self.node_id.as_deref() {
                Some(wanted) => node_id == Some(wanted),
                None => true,
            }
    }
}

/// Whether `glob` matches `value`, where `*` matches any run of characters.
///
/// Deliberately just `*`: tool slugs and URLs are the things being matched, and
/// a full regex dependency to express `gh.issues.*` would be a poor trade.
fn glob_matches(glob: &str, value: &str) -> bool {
    if glob == "*" {
        return true;
    }
    if !glob.contains('*') {
        return glob == value;
    }
    let parts: Vec<&str> = glob.split('*').collect();
    let mut rest = value;
    for (index, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        match index {
            // A leading literal must sit at the start.
            0 => match rest.strip_prefix(part) {
                Some(tail) => rest = tail,
                None => return false,
            },
            _ => match rest.find(part) {
                Some(at) => rest = &rest[at + part.len()..],
                None => return false,
            },
        }
    }
    // A glob with no trailing `*` must have consumed everything.
    parts
        .last()
        .is_none_or(|last| last.is_empty() || rest.is_empty())
}

/// One programmed rule and how many times it has answered.
#[derive(Debug)]
struct Rule {
    matcher: Matcher,
    respond: Respond,
    hits: AtomicU64,
}

/// A programmable, recording set of capability doubles.
///
/// Build one with the `on_*` methods, hand [`capabilities`](Self::capabilities)
/// to a run, then read [`log`](Self::log) afterwards.
#[derive(Debug, Default)]
pub struct MockCaps {
    rules: Vec<Rule>,
    log: Arc<CallLog>,
    workflows: HashMap<String, WorkflowGraph>,
    /// Backing map for the [`StateStore`](crate::caps::StateStore) impl.
    ///
    /// Lives here, not on each [`Double`](double::Double), because
    /// [`capabilities_for_node`](Self::capabilities_for_node) builds a fresh
    /// `Double` per node activation (so the call log can attribute calls to
    /// the right node); a per-`Double` map would make state invisible across
    /// activations — including a node's own later activation — defeating the
    /// one job a state store has.
    state: Mutex<HashMap<String, Value>>,
}

impl MockCaps {
    /// A set of doubles with no rules: every call falls back to the echo
    /// behaviour, and every call is logged.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The call log these doubles write to.
    #[must_use]
    pub fn log(&self) -> Arc<CallLog> {
        self.log.clone()
    }

    /// Program a rule.
    #[must_use]
    fn rule(mut self, capability: &str, target: &str, respond: Respond) -> Self {
        self.rules.push(Rule {
            matcher: Matcher {
                capability: capability.to_string(),
                target: target.to_string(),
                node_id: None,
            },
            respond,
            hits: AtomicU64::new(0),
        });
        self
    }

    /// Answer `slug` (a glob) on the tool invoker.
    #[must_use]
    pub fn on_tool(self, slug: &str, respond: Respond) -> Self {
        self.rule(capability::TOOLS, slug, respond)
    }

    /// Answer HTTP requests whose URL matches `url` (a glob).
    #[must_use]
    pub fn on_http(self, url: &str, respond: Respond) -> Self {
        self.rule(capability::HTTP, url, respond)
    }

    /// Answer LLM completions.
    #[must_use]
    pub fn on_llm(self, respond: Respond) -> Self {
        self.rule(capability::LLM, "*", respond)
    }

    /// Answer the agent runner for `agent_ref` (a glob).
    #[must_use]
    pub fn on_agent(self, agent_ref: &str, respond: Respond) -> Self {
        self.rule(capability::AGENT, agent_ref, respond)
    }

    /// Answer code execution.
    #[must_use]
    pub fn on_code(self, respond: Respond) -> Self {
        self.rule(capability::CODE, "*", respond)
    }

    /// Answer shell execution.
    #[must_use]
    pub fn on_shell(self, respond: Respond) -> Self {
        self.rule(capability::SHELL, "*", respond)
    }

    /// Answer human review for the requests whose `request_id` matches
    /// `request_id` (a glob).
    ///
    /// The answer is read loosely, the way [`on_shell`](Self::on_shell)'s is:
    /// `{"approved": false, "comment": "…"}` is a rejection, `{"approved":
    /// true}` an approval, and `{"status": "pending"}` a review nobody has got
    /// to yet — which is how a test exercises a `poll`ing review or the
    /// suspend/resume path. `Respond::error` fails the call instead.
    #[must_use]
    pub fn on_approval(self, request_id: &str, respond: Respond) -> Self {
        self.rule(capability::APPROVALS, request_id, respond)
    }

    /// Restrict the most recently programmed rule to calls made by `node_id`.
    ///
    /// This is what per-node mocking looks like: stub one node's tool calls and
    /// leave the rest of the graph alone.
    ///
    /// A no-op when no rule has been programmed yet.
    #[must_use]
    pub fn only_from(mut self, node_id: &str) -> Self {
        if let Some(rule) = self.rules.last_mut() {
            rule.matcher.node_id = Some(node_id.to_string());
        }
        self
    }

    /// Register a graph a `sub_workflow` node can resolve by id.
    #[must_use]
    pub fn with_workflow(mut self, id: impl Into<String>, graph: WorkflowGraph) -> Self {
        self.workflows.insert(id.into(), graph);
        self
    }

    /// Find the answer for a call, or `None` to fall back to the default.
    async fn respond_to(
        &self,
        capability: &str,
        target: &str,
        node_id: Option<&str>,
        request: &Value,
    ) -> Option<Result<Value>> {
        for rule in &self.rules {
            if rule.matcher.matches(capability, target, node_id) {
                let hit = rule.hits.fetch_add(1, Ordering::SeqCst) as usize;
                return Some(rule.respond.answer(hit, request).await);
            }
        }
        None
    }

    /// The capability bundle to hand a run.
    ///
    /// Every slot is filled, so a graph reaching a capability nobody programmed
    /// gets the echo rather than a "capability not configured" failure that
    /// would say nothing about the graph.
    #[must_use]
    pub fn capabilities(self: &Arc<Self>) -> Capabilities {
        let shared = self.clone();
        Capabilities {
            llm: Arc::new(Double::new(shared.clone(), None)),
            tools: Arc::new(Double::new(shared.clone(), None)),
            http: Arc::new(Double::new(shared.clone(), None)),
            code: Arc::new(Double::new(shared.clone(), None)),
            state: Arc::new(Double::new(shared.clone(), None)),
            resolver: Arc::new(Double::new(shared.clone(), None)),
            agent: Some(Arc::new(Double::new(shared.clone(), None))),
            shell: Some(Arc::new(Double::new(shared.clone(), None))),
            memory: Some(Arc::new(Double::new(shared.clone(), None))),
            tasks: Some(Arc::new(crate::caps::TokioTaskRunner::new())),
            approvals: Some(Arc::new(Double::new(shared, None))),
        }
    }

    /// The same bundle, with every call it makes attributed to `node_id`.
    ///
    /// Used through
    /// [`StepInterceptor::capabilities_for`](crate::interception::StepInterceptor::capabilities_for)
    /// so the log can say which node made which call.
    #[must_use]
    pub fn capabilities_for_node(self: &Arc<Self>, node_id: &str) -> Capabilities {
        let shared = self.clone();
        let node = Some(node_id.to_string());
        Capabilities {
            llm: Arc::new(Double::new(shared.clone(), node.clone())),
            tools: Arc::new(Double::new(shared.clone(), node.clone())),
            http: Arc::new(Double::new(shared.clone(), node.clone())),
            code: Arc::new(Double::new(shared.clone(), node.clone())),
            state: Arc::new(Double::new(shared.clone(), node.clone())),
            resolver: Arc::new(Double::new(shared.clone(), node.clone())),
            agent: Some(Arc::new(Double::new(shared.clone(), node.clone()))),
            shell: Some(Arc::new(Double::new(shared.clone(), node.clone()))),
            memory: Some(Arc::new(Double::new(shared.clone(), node.clone()))),
            tasks: Some(Arc::new(crate::caps::TokioTaskRunner::new())),
            approvals: Some(Arc::new(Double::new(shared, node))),
        }
    }
}

#[cfg(test)]
#[path = "mocks_tests.rs"]
mod tests;
