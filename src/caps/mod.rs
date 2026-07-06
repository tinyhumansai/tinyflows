//! Host-injected capability traits.
//!
//! tinyflows stays host-agnostic: everything that touches the outside world is
//! expressed as a trait the embedding application implements. OpenHuman's
//! adapter seam (`src/openhuman/tinyflows/`) wires these to its inference stack,
//! curated Composio tools, `HttpRequestTool`, and sandboxed code runtimes.

#[cfg(any(test, feature = "mock"))]
pub mod mock;

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::error::Result;

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

/// Runs a host-registered, multi-turn **agent** identified by a stable id.
///
/// Where [`LlmProvider::complete`] is a single chat call, an `AgentRunner` runs
/// a *named agent kind* the host has defined — a coding agent, a researcher, a
/// crypto agent — each bringing its own curated tool access, model, sandbox, and
/// iteration policy. An `agent` node selects one via a trusted `agent_ref` in its
/// config; when the host wires this capability, the node runs that agent to
/// completion instead of issuing a bare completion.
///
/// The engine stays host-agnostic: `agent_ref` is an opaque id the host resolves
/// against its own agent registry. This capability is **optional** — hosts
/// without an agent registry leave [`Capabilities::agent`] `None`, and `agent`
/// nodes fall back to [`LlmProvider`].
#[async_trait]
pub trait AgentRunner: Send + Sync {
    /// Runs the host-registered agent identified by `agent_ref` to completion.
    ///
    /// `request` is the (expression-resolved) node config — prompt/input plus any
    /// per-node overrides the host chooses to honor (e.g. a narrowing tool
    /// allow-list). `conn` is the same opaque, host-managed connection reference
    /// the other capabilities take. The returned JSON is treated exactly like a
    /// completion response by the `agent` node (enveloped into `{ json, text,
    /// raw }`), so downstream binding is identical to a plain agent turn.
    ///
    /// # Errors
    /// Returns an [`EngineError::Capability`](crate::error::EngineError::Capability)
    /// when `agent_ref` is unknown or the agent run fails.
    async fn run_agent(&self, agent_ref: &str, request: Value, conn: Option<&str>)
    -> Result<Value>;
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
/// all five host-injected capabilities: the [`LlmProvider`], [`ToolInvoker`],
/// [`HttpClient`], [`CodeRunner`], and [`StateStore`]. Nodes reach each one
/// through `ctx.caps` during execution.
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::mock::mock_capabilities;
    use serde_json::json;

    #[test]
    fn code_language_is_copy_and_comparable() {
        let js = CodeLanguage::JavaScript;
        let copied = js; // `Copy`, so `js` remains usable below.
        assert_eq!(js, copied);
        assert_eq!(js, CodeLanguage::JavaScript);
        assert_ne!(CodeLanguage::JavaScript, CodeLanguage::Python);
    }

    #[test]
    fn code_language_debug_names_the_variant() {
        assert_eq!(format!("{:?}", CodeLanguage::JavaScript), "JavaScript");
        assert_eq!(format!("{:?}", CodeLanguage::Python), "Python");
    }

    #[test]
    fn capabilities_bundle_is_cloneable_and_shares_impls() {
        let caps = mock_capabilities();
        let clone = caps.clone();
        // Cloning shares the underlying `Arc`s rather than deep-copying.
        assert!(Arc::ptr_eq(&caps.llm, &clone.llm));
        assert!(Arc::ptr_eq(&caps.tools, &clone.tools));
        assert!(Arc::ptr_eq(&caps.http, &clone.http));
        assert!(Arc::ptr_eq(&caps.code, &clone.code));
        assert!(Arc::ptr_eq(&caps.state, &clone.state));
        assert!(Arc::ptr_eq(&caps.resolver, &clone.resolver));
    }

    #[tokio::test]
    async fn capabilities_dispatch_through_trait_objects() {
        // Exercises the bundle purely through the trait-object surface.
        let caps = mock_capabilities();
        let out = caps
            .llm
            .complete(json!({"prompt": "hi"}), None)
            .await
            .unwrap();
        assert_eq!(out["completion"]["prompt"], "hi");
    }
}
