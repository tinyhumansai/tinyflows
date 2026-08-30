//! Host-injected capability traits.
//!
//! tinyflows stays host-agnostic: everything that touches the outside world is
//! expressed as a trait the embedding application implements. OpenHuman's
//! adapter seam (`src/openhuman/tinyflows/`) wires these to its inference stack,
//! curated Composio tools, `HttpRequestTool`, and sandboxed code runtimes.

pub mod agent;
pub mod approval;
#[cfg(any(test, feature = "host-caps"))]
pub mod host;
#[cfg(any(test, feature = "mock"))]
pub mod mock;
// Kept apart from `mock` because it answers a different question. `mock` echoes,
// which is what an *engine* test wants; this synthesises the shape a node
// declared, which is what a *graph* dry run wants. Mixing them would make it a
// coin flip which one a caller reached for, and the wrong choice fails a good
// graph.
#[cfg(any(test, feature = "mock"))]
pub mod mock_schema_aware;
pub mod schema;
pub mod shell;
pub mod tasks;

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::error::Result;

pub use self::agent::{
    AgentInput, AgentModelSelection, AgentRunIdentity, AgentRunOutcome, AgentRunRequest,
    AgentRunner, AgentUsage, ContextBlock, StopReason, ToolDescriptor, WorkdirCheck,
};
pub use self::approval::{
    ApprovalDecision, ApprovalOutcome, ApprovalProvider, ApprovalRequest, ApprovalSubject,
};
pub use self::schema::sample_for_schema;
pub use self::shell::{ShellInterpreter, ShellOutcome, ShellRequest, ShellRunner, ShellScript};
pub use self::tasks::{TaskRunner, TaskSpec, TaskState, TokioTaskRunner};

/// A chat / LLM provider used by `agent` and `output_parser` nodes.
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Runs a single completion given a JSON request, returning a JSON response.
    ///
    /// `conn` is an optional opaque, host-managed connection reference (e.g. a
    /// provider credential id) that names the account the call acts as; the host
    /// resolves it to real secrets inside this implementation.
    async fn complete(&self, request: Value, conn: Option<&str>) -> Result<Value>;
}

/// Host-injected memory access for `memory` nodes.
///
/// A `memory` node is a **delivery surface** onto whatever durable memory store
/// the host already maintains — it exposes no capability of its own. Six config
/// `operation`s map onto five trait methods:
///
/// - `recall` and `search` both call [`recall`](Self::recall); `search` passes
///   `opts.operation == "search"` (alongside the same `query`) so a host that
///   distinguishes semantic recall from full-text search can branch on it, but
///   a host that treats them identically may ignore the distinction entirely.
///   This keeps the trait small (one read-with-a-query method rather than two
///   near-duplicates) while still letting `opts` carry the distinction through
///   to the host.
/// - `flavour` and `people` are read-only lookups with no free-text `query`
///   requirement, each with its own method.
/// - `remember` and `forget` are the two write operations.
///
/// `scope` is an opaque, host-defined string (the model layer never interprets
/// it) but the wire contract in practice is `"user"` (the caller's durable,
/// cross-flow memory — read-only from a workflow), `"flow"` (this flow's own
/// memory — the only scope `remember`/`forget` may target), and `"flows"`
/// (cross-flow read access — read-only). [`crate::validate`] enforces the hard
/// invariant that a `remember`/`forget` operation may never carry
/// `scope: "user"`, structurally, before a run ever starts — that boundary is
/// NOT re-checked here, so a host implementing this trait directly (bypassing
/// the node + validator) must not treat that check as already done.
///
/// This capability is **optional**, matching the [`AgentRunner`] precedent:
/// hosts without a memory store leave [`Capabilities::memory`] `None`, and a
/// `memory` node then fails at run time with a capability error rather than
/// silently no-opping (there is no meaningful fallback for a read/write memory
/// call the way `agent` falls back to a bare completion).
#[async_trait]
pub trait MemoryProvider: Send + Sync {
    /// Recalls memory matching `query` within `scope`.
    ///
    /// Backs both the `recall` and `search` node operations; `opts` carries
    /// `"operation"` (`"recall"` or `"search"`) plus any of the node's optional
    /// `limit` / `min_score` config, each present only when the author set it.
    /// The return shape is host-defined — typically an object with a `results`
    /// (or similar) array — and is passed through to the node's output
    /// envelope unchanged.
    ///
    /// # Errors
    /// Returns an [`EngineError::Capability`](crate::error::EngineError::Capability)
    /// when the scope is unknown to the host or the lookup fails.
    async fn recall(&self, scope: &str, query: &str, opts: Value) -> Result<Value>;

    /// Looks up a named "flavour" (a host-defined ask/persona/style profile,
    /// e.g. `"email-tone"`) by its slug.
    ///
    /// # Errors
    /// Returns an [`EngineError::Capability`](crate::error::EngineError::Capability)
    /// when `slug` is unknown to the host.
    async fn flavour(&self, slug: &str) -> Result<Value>;

    /// Looks up people the host's memory knows about, optionally narrowed by a
    /// free-text `query`; `None` returns the host's default listing.
    ///
    /// # Errors
    /// Returns an [`EngineError::Capability`](crate::error::EngineError::Capability)
    /// when the lookup fails.
    async fn people(&self, query: Option<&str>) -> Result<Value>;

    /// Persists `value` under `key` within `scope`.
    ///
    /// Callers (the `memory` node, via the validator) must never pass
    /// `scope: "user"` here — see the trait-level docs. `value` is caller
    /// (workflow-author / upstream node) controlled; hosts should apply the
    /// same taint/provenance handling they use for any externally-influenced
    /// write.
    ///
    /// # Errors
    /// Returns an [`EngineError::Capability`](crate::error::EngineError::Capability)
    /// when the scope is unknown to the host or the write fails.
    async fn remember(&self, scope: &str, key: &str, value: Value) -> Result<()>;

    /// Deletes the memory stored under `key` within `scope`.
    ///
    /// Same `scope: "user"` restriction as [`remember`](Self::remember).
    /// Deleting an already-absent `key` is not an error.
    ///
    /// # Errors
    /// Returns an [`EngineError::Capability`](crate::error::EngineError::Capability)
    /// when the scope is unknown to the host or the delete fails.
    async fn forget(&self, scope: &str, key: &str) -> Result<()>;
}

/// Invokes a named integration tool (e.g. a curated Composio action).
#[async_trait]
pub trait ToolInvoker: Send + Sync {
    /// Executes the tool identified by `slug` with `args`, returning its output.
    ///
    /// `conn` is an optional opaque, host-managed connection reference (e.g. a
    /// Composio connection id) that names the account the call acts as; the host
    /// resolves it to real secrets inside this implementation.
    async fn invoke(&self, slug: &str, args: Value, conn: Option<&str>) -> Result<Value>;
}

/// Performs an outbound HTTP request on behalf of an `http_request` node.
#[async_trait]
pub trait HttpClient: Send + Sync {
    /// Issues the request described by `request`, returning the response as JSON.
    ///
    /// `conn` is an optional opaque, host-managed connection reference (e.g. an
    /// HTTP credential id) that names the account the call acts as; the host
    /// resolves it to real secrets inside this implementation.
    async fn request(&self, request: Value, conn: Option<&str>) -> Result<Value>;
}

/// The language a [`CodeRunner`] should execute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeLanguage {
    /// JavaScript, executed out-of-process via a managed Node toolchain.
    JavaScript,
    /// Python, executed out-of-process via a managed CPython toolchain.
    Python,
}

/// Runs sandboxed user code for a `code` node.
#[async_trait]
pub trait CodeRunner: Send + Sync {
    /// Executes `source` in `language` with `input`, returning its JSON output.
    async fn run(&self, language: CodeLanguage, source: &str, input: Value) -> Result<Value>;
}

/// Resolves a saved workflow graph by its host-managed id.
///
/// A [`sub_workflow`](crate::nodes::integration) node may reference a child by a
/// host `workflow_id` instead of embedding the child graph inline. The engine is
/// deliberately persistence-free, so it delegates that lookup to this
/// host-injected capability: the embedding application maps `workflow_id` onto
/// whatever store it keeps saved workflows in and hands back the portable
/// [`WorkflowGraph`](crate::model::WorkflowGraph) to run.
#[async_trait]
pub trait WorkflowResolver: Send + Sync {
    /// Resolves `workflow_id` to the child [`WorkflowGraph`](crate::model::WorkflowGraph)
    /// the referencing `sub_workflow` node should compile and run.
    ///
    /// # Errors
    /// Returns an [`EngineError::Capability`](crate::error::EngineError::Capability)
    /// when no workflow with that id exists, or when the host cannot load it.
    async fn resolve(&self, workflow_id: &str) -> Result<crate::model::WorkflowGraph>;
}

/// Durable key/value state for a run (used by resumable / stateful workflows).
#[async_trait]
pub trait StateStore: Send + Sync {
    /// Loads a previously stored value by key.
    async fn load(&self, key: &str) -> Result<Option<Value>>;
    /// Persists a value under `key`.
    async fn store(&self, key: &str, value: Value) -> Result<()>;
}

/// The bundle of capabilities handed to the engine for a run.
///
/// Construct one per run from the host's concrete implementations. It carries
/// every host-injected capability: the always-present [`LlmProvider`],
/// [`ToolInvoker`], [`HttpClient`], [`CodeRunner`], [`StateStore`], and
/// [`WorkflowResolver`], plus the optional [`AgentRunner`], [`MemoryProvider`],
/// and [`ApprovalProvider`]. Nodes reach each one through `ctx.caps` during
/// execution.
#[derive(Clone)]
pub struct Capabilities {
    /// LLM provider for agent / output-parser nodes.
    pub llm: Arc<dyn LlmProvider>,
    /// Integration tool invoker for `tool_call` nodes.
    pub tools: Arc<dyn ToolInvoker>,
    /// Outbound HTTP client for `http_request` nodes.
    pub http: Arc<dyn HttpClient>,
    /// Sandboxed code runner for `code` nodes.
    pub code: Arc<dyn CodeRunner>,
    /// Durable key/value state store for stateful workflows.
    ///
    /// The host implements [`StateStore`] (for example, OpenHuman's
    /// run-ledger-backed store) and nodes access durable state through
    /// `ctx.caps.state`.
    pub state: Arc<dyn StateStore>,
    /// Resolver for `sub_workflow` nodes that reference a child graph by a
    /// host `workflow_id` rather than embedding it inline. The host implements
    /// [`WorkflowResolver`] over its saved-workflow store; the engine calls it
    /// only when a `sub_workflow` node carries a `workflow_id`.
    pub resolver: Arc<dyn WorkflowResolver>,
    /// Optional runner for host-registered **agent kinds**. When set, an `agent`
    /// node whose config carries a trusted `agent_ref` runs that named agent
    /// (with its own tools/model/sandbox) via [`AgentRunner`] instead of a bare
    /// [`LlmProvider`] completion. `None` on hosts without an agent registry, in
    /// which case `agent` nodes always use [`LlmProvider`].
    pub agent: Option<Arc<dyn AgentRunner>>,
    /// Optional runner for `shell` nodes. `None` on hosts that do not permit
    /// workflows to execute shell scripts, in which case a `shell` node fails
    /// with a capability error rather than silently doing nothing.
    pub shell: Option<Arc<dyn ShellRunner>>,
    /// Optional host-managed memory access for `memory` nodes. `None` on hosts
    /// without a memory store, in which case a `memory` node fails at run time
    /// with a capability error (there is no meaningful no-op fallback for a
    /// read/write memory call). See [`MemoryProvider`] for the `scope`
    /// contract and the `remember`/`forget` write restriction.
    pub memory: Option<Arc<dyn MemoryProvider>>,
    /// Runner for background work a `spawn` node starts and a `gate` node
    /// collects.
    ///
    /// Defaults to [`TokioTaskRunner`], so spawn/gate genuinely overlap without
    /// a host wiring anything. `None` makes `spawn` run its work **inline** and
    /// hand back a ticket that is already settled: such a graph still produces
    /// the right answer, it just loses the overlap. That is a performance cliff
    /// rather than a correctness one, which is exactly why it is worth saying
    /// out loud here and in the node catalog.
    pub tasks: Option<Arc<dyn TaskRunner>>,
    /// Optional human-review surface for `approval` nodes. `None` on hosts
    /// without one, in which case an `approval` node falls back to pausing the
    /// run and letting the host settle it through
    /// [`engine::resume`](crate::engine::resume) — so the node still works, it
    /// just has no way to *push* the request at anybody. See
    /// [`ApprovalProvider`] for the create-or-fetch contract.
    pub approvals: Option<Arc<dyn ApprovalProvider>>,
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
