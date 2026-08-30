//! In-memory mock capability implementations for tests and examples.
//!
//! Enabled inside this crate's own tests automatically, or downstream via the
//! `mock` cargo feature. The mocks are deterministic echoes — enough to exercise
//! the engine and the reference workflows without any external services.

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::caps::{
    AgentRunner, CodeLanguage, CodeRunner, HttpClient, LlmProvider, MemoryProvider, ShellOutcome,
    ShellRequest, ShellRunner, ShellScript, StateStore, ToolInvoker, WorkflowResolver,
};
use crate::error::{EngineError, Result};
use crate::model::WorkflowGraph;

#[path = "mock_approvals.rs"]
mod mock_approvals;
pub use mock_approvals::MockApprovals;

/// An [`LlmProvider`] that echoes the request back under a `completion` key.
#[derive(Debug, Default, Clone)]
pub struct MockLlm;

#[async_trait]
impl LlmProvider for MockLlm {
    async fn complete(&self, request: Value, conn: Option<&str>) -> Result<Value> {
        Ok(json!({ "completion": request, "connection": conn }))
    }
}

/// An [`AgentRunner`] that echoes the `agent_ref`, request, and connection it was
/// invoked with — enough to assert an `agent` node routed to a named agent kind.
#[derive(Debug, Default, Clone)]
pub struct MockAgentRunner;

#[async_trait]
impl AgentRunner for MockAgentRunner {
    async fn run_agent(
        &self,
        agent_ref: &str,
        request: Value,
        conn: Option<&str>,
    ) -> Result<Value> {
        Ok(json!({ "agent": agent_ref, "request": request, "connection": conn }))
    }
}

/// An [`AgentRunner`] that implements the **typed** seam: it echoes the
/// assembled [`AgentRunRequest`](crate::caps::AgentRunRequest) rather than the
/// raw config, and can answer registry and context lookups.
///
/// Where [`MockAgentRunner`] stands in for a host written against the previous
/// release (only `run_agent`, so the default `run` shim applies), this one
/// stands in for a harness that has opted in — which is what a test asserting
/// merge semantics, context resolution, or tool narrowing needs to see.
///
/// The echo is deliberately *shaped* rather than a blob, so a downstream
/// `condition` or `transform` in a dry run has stable fields to bind to.
#[derive(Debug, Default, Clone)]
pub struct MockAgentHarness {
    /// Agent kinds this mock registry knows, consulted by
    /// [`resolve_agent`](AgentRunner::resolve_agent) when the graph does not
    /// declare the ref itself.
    pub agents: Vec<crate::model::AgentDefinition>,
}

impl MockAgentHarness {
    /// A harness with an empty registry — every `resolve_agent` misses, so refs
    /// pass through as bare ids.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers an agent kind, mirroring [`MockWorkflowResolver::with`].
    #[must_use]
    pub fn with(mut self, agent: crate::model::AgentDefinition) -> Self {
        self.agents.push(agent);
        self
    }
}

#[async_trait]
impl AgentRunner for MockAgentHarness {
    async fn run_agent(
        &self,
        agent_ref: &str,
        request: Value,
        conn: Option<&str>,
    ) -> Result<Value> {
        Ok(json!({ "agent": agent_ref, "request": request, "connection": conn }))
    }

    async fn run(
        &self,
        request: crate::caps::AgentRunRequest,
    ) -> Result<crate::caps::AgentRunOutcome> {
        let echo = json!({
            "agent": request.agent.id,
            "instructions": request.agent.instructions,
            "model": request.model.model,
            "provider": request.model.provider,
            "working_dir": request.working_dir,
            "limits": request.agent.limits,
            "metadata": request.metadata,
            "prompt": request.input.text,
            "data": request.input.data,
            "context": request
                .context
                .iter()
                .map(|b| json!({ "label": b.label, "kind": b.source_kind, "text": b.text, "data": b.data }))
                .collect::<Vec<_>>(),
            "tools": request.tools.iter().map(|t| t.slug.clone()).collect::<Vec<_>>(),
            "connection": request.connection_ref,
            "identity": request.identity,
        });
        Ok(crate::caps::AgentRunOutcome {
            stop: crate::caps::StopReason::Finished,
            text: Some(format!("ran {}", request.agent.id)),
            json: echo.clone(),
            raw: echo,
            usage: Some(crate::caps::AgentUsage {
                steps: Some(1),
                ..Default::default()
            }),
            // Non-empty on purpose: a host testing against this mock should see
            // a transcript reach its observer, not an empty vec that passes for
            // the same thing.
            transcript: vec![
                crate::transcript::TranscriptEntry::bounded(
                    0,
                    "agent_thinking",
                    format!("deciding how to answer as {}", request.agent.id),
                ),
                crate::transcript::TranscriptEntry::bounded(
                    0,
                    "agent_message",
                    format!("ran {}", request.agent.id),
                ),
            ],
        })
    }

    async fn resolve_agent(
        &self,
        agent_ref: &str,
    ) -> Result<Option<crate::model::AgentDefinition>> {
        Ok(self.agents.iter().find(|a| a.id == agent_ref).cloned())
    }

    async fn list_agents(&self) -> Result<Vec<crate::model::AgentDefinition>> {
        Ok(self.agents.clone())
    }

    async fn resolve_context(
        &self,
        source: &str,
        params: &Value,
        _identity: &crate::caps::AgentRunIdentity,
    ) -> Result<Option<crate::caps::ContextBlock>> {
        // `"unknown"` is the one source this mock refuses, so a test can cover
        // the `Ok(None)` path (and the `optional` policy that rides on it)
        // without needing a second mock.
        if source == "unknown" {
            return Ok(None);
        }
        Ok(Some(crate::caps::ContextBlock {
            label: source.to_string(),
            source_kind: "host".to_string(),
            text: Some(format!("mock context for {source:?}")),
            data: params.clone(),
        }))
    }

    async fn resolve_tools(
        &self,
        grants: &[crate::model::ToolGrant],
        conn: Option<&str>,
    ) -> Result<Vec<crate::caps::ToolDescriptor>> {
        // Expands a trailing-`.*` pattern into two concrete slugs, so a test can
        // tell an expanded grant from a passed-through one.
        Ok(grants
            .iter()
            .flat_map(|grant| {
                match grant.slug.strip_suffix(".*") {
                    Some(ns) => vec![format!("{ns}.alpha"), format!("{ns}.beta")],
                    None => vec![grant.slug.clone()],
                }
                .into_iter()
                .map(|slug| crate::caps::ToolDescriptor {
                    description: Some(format!("mock tool {slug}")),
                    input_schema: Some(json!({ "type": "object" })),
                    connection_ref: grant
                        .connection_ref
                        .clone()
                        .or_else(|| conn.map(str::to_string)),
                    name: None,
                    slug,
                })
                .collect::<Vec<_>>()
            })
            .collect())
    }
}

/// An [`AgentRunner`] whose loop always stops on a limit, keeping partial
/// output — for tests covering the `limit_stop` path (`meta.stop`, and the
/// output parser being skipped on a knowingly partial payload).
#[derive(Debug, Default, Clone)]
pub struct MockLimitedAgentRunner;

#[async_trait]
impl AgentRunner for MockLimitedAgentRunner {
    async fn run_agent(
        &self,
        _agent_ref: &str,
        _request: Value,
        _conn: Option<&str>,
    ) -> Result<Value> {
        Ok(Value::Null)
    }

    async fn run(
        &self,
        request: crate::caps::AgentRunRequest,
    ) -> Result<crate::caps::AgentRunOutcome> {
        let partial = json!({ "partial": true, "agent": request.agent.id });
        Ok(crate::caps::AgentRunOutcome {
            stop: crate::caps::StopReason::LimitStop {
                limit: "max_steps".to_string(),
            },
            text: Some("got as far as I could".to_string()),
            json: partial.clone(),
            raw: partial,
            usage: None,
            transcript: vec![crate::transcript::TranscriptEntry::bounded(
                0,
                "agent_thinking",
                "ran out of steps mid-thought",
            )],
        })
    }
}

/// An [`AgentRunner`] whose loop always pauses for a human — for tests covering
/// the (currently unsupported) resume path failing loudly rather than emitting a
/// half-run agent's output as an answer.
#[derive(Debug, Default, Clone)]
pub struct MockPausingAgentRunner;

#[async_trait]
impl AgentRunner for MockPausingAgentRunner {
    async fn run_agent(
        &self,
        _agent_ref: &str,
        _request: Value,
        _conn: Option<&str>,
    ) -> Result<Value> {
        Ok(Value::Null)
    }

    async fn run(
        &self,
        _request: crate::caps::AgentRunRequest,
    ) -> Result<crate::caps::AgentRunOutcome> {
        Ok(crate::caps::AgentRunOutcome {
            stop: crate::caps::StopReason::Paused {
                token: Some("mock_pause_1".to_string()),
                reason: "tool_approval".to_string(),
                payload: json!({ "tool": "github.add_labels" }),
            },
            text: None,
            json: Value::Null,
            raw: Value::Null,
            usage: None,
            // A pause still explains itself: this is the run whose transcript is
            // most worth reading, because its output never arrives.
            transcript: vec![crate::transcript::TranscriptEntry::bounded(
                0,
                "tool_call",
                "github.add_labels (awaiting approval)",
            )],
        })
    }
}

/// A [`ToolInvoker`] that echoes the slug and args it was called with.
#[derive(Debug, Default, Clone)]
pub struct MockTools;

#[async_trait]
impl ToolInvoker for MockTools {
    async fn invoke(&self, slug: &str, args: Value, conn: Option<&str>) -> Result<Value> {
        Ok(json!({ "tool": slug, "args": args, "connection": conn }))
    }
}

/// An [`HttpClient`] that returns a canned `200` response echoing the request.
#[derive(Debug, Default, Clone)]
pub struct MockHttp;

#[async_trait]
impl HttpClient for MockHttp {
    async fn request(&self, request: Value, conn: Option<&str>) -> Result<Value> {
        Ok(json!({ "status": 200, "request": request, "connection": conn }))
    }
}

/// A [`CodeRunner`] that returns its input unchanged under a `result` key.
#[derive(Debug, Default, Clone)]
pub struct MockCode;

#[async_trait]
impl CodeRunner for MockCode {
    async fn run(&self, _language: CodeLanguage, _source: &str, input: Value) -> Result<Value> {
        Ok(json!({ "result": input }))
    }
}

/// A [`ShellRunner`] that never spawns anything and echoes the request instead.
///
/// Always succeeds. Standard output is the script text (inline source, or the
/// path as written), so a test can assert which script a `shell` node asked
/// for; standard error carries the interpreter, working directory, and
/// environment. A test that needs a failing script supplies its own runner —
/// baking a magic "fail" script into the mock would make an author's real
/// script mean something different during a dry run.
#[derive(Debug, Default, Clone)]
pub struct MockShell;

#[async_trait]
impl ShellRunner for MockShell {
    async fn run(&self, request: ShellRequest) -> Result<ShellOutcome> {
        Ok(ShellOutcome {
            exit_code: 0,
            stdout: match &request.script {
                ShellScript::Inline(source) => source.clone(),
                ShellScript::Path(path) => path.clone(),
            },
            stderr: format!(
                "{} cwd={} env={}",
                request.interpreter.as_str(),
                request.cwd.unwrap_or_default(),
                json!(request.env),
            ),
        })
    }
}

/// A [`MemoryProvider`] returning small, deterministic, **shaped** canned data.
///
/// Unlike a naive echo mock, each method returns a plausible result shape (a
/// `results` array for recall/search, a `traits` object for flavour, a `people`
/// array for people lookups) rather than `null`/an empty object — the same
/// "shaped mock" precedent OpenHuman's `SchemaAwareMockAgentRunner` /
/// `SchemaAwareMockLlm` set, because a naive mock made dry-runs meaningless
/// (downstream `condition`/`transform` nodes had nothing to bind to). Wired
/// into [`mock_capabilities`] by default (not behind an opt-in helper, unlike
/// [`MockAgentRunner`]) precisely so a workflow containing a `memory` node
/// dry-runs out of the box.
#[derive(Debug, Default, Clone)]
pub struct MockMemory;

#[async_trait]
impl MemoryProvider for MockMemory {
    async fn recall(&self, scope: &str, query: &str, opts: Value) -> Result<Value> {
        Ok(json!({
            "scope": scope,
            "query": query,
            "opts": opts,
            "results": [
                { "id": "mem_1", "text": format!("mock memory matching '{query}'"), "score": 0.92 },
                { "id": "mem_2", "text": "a second mock memory", "score": 0.81 },
            ],
        }))
    }

    async fn flavour(&self, slug: &str) -> Result<Value> {
        Ok(json!({
            "slug": slug,
            "summary": format!("mock flavour profile for '{slug}'"),
            "traits": { "tone": "warm", "formality": "casual" },
        }))
    }

    async fn people(&self, query: Option<&str>) -> Result<Value> {
        Ok(json!({
            "query": query,
            "people": [
                { "id": "person_1", "name": "Mock Person A" },
                { "id": "person_2", "name": "Mock Person B" },
            ],
        }))
    }

    async fn remember(&self, _scope: &str, _key: &str, _value: Value) -> Result<()> {
        Ok(())
    }

    async fn forget(&self, _scope: &str, _key: &str) -> Result<()> {
        Ok(())
    }
}

/// A [`StateStore`] backed by an in-memory map guarded by a mutex.
#[derive(Debug, Default)]
pub struct MockStateStore {
    inner: std::sync::Mutex<std::collections::HashMap<String, Value>>,
}

#[async_trait]
impl StateStore for MockStateStore {
    async fn load(&self, key: &str) -> Result<Option<Value>> {
        Ok(self.inner.lock().expect("lock").get(key).cloned())
    }

    async fn store(&self, key: &str, value: Value) -> Result<()> {
        self.inner
            .lock()
            .expect("lock")
            .insert(key.to_string(), value);
        Ok(())
    }
}

/// A [`WorkflowResolver`] backed by an in-memory `workflow_id` → graph map.
///
/// Empty by default (every `resolve` misses with a capability error), so a run
/// that never uses `sub_workflow`-by-id is unaffected. Register graphs with
/// [`MockWorkflowResolver::with`] to exercise the by-id path in tests.
#[derive(Debug, Default, Clone)]
pub struct MockWorkflowResolver {
    workflows: std::collections::HashMap<String, WorkflowGraph>,
}

impl MockWorkflowResolver {
    /// Registers `graph` under `id`, returning `self` for chaining.
    #[must_use]
    pub fn with(mut self, id: impl Into<String>, graph: WorkflowGraph) -> Self {
        self.workflows.insert(id.into(), graph);
        self
    }
}

#[async_trait]
impl WorkflowResolver for MockWorkflowResolver {
    async fn resolve(&self, workflow_id: &str) -> Result<WorkflowGraph> {
        self.workflows.get(workflow_id).cloned().ok_or_else(|| {
            EngineError::Capability(format!("mock resolver: unknown workflow_id: {workflow_id}"))
        })
    }
}

#[path = "mock_builders.rs"]
mod mock_builders;
pub use mock_builders::{
    mock_capabilities, mock_capabilities_with_agent, mock_capabilities_with_approvals,
    mock_capabilities_with_memory, mock_capabilities_with_resolver,
};

#[cfg(test)]
#[path = "mock_tests.rs"]
mod tests;
