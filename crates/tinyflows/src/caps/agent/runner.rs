use super::*;

/// A host's answer to "where does this declared working directory actually
/// live?" — see [`AgentRunner::resolve_workdir`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkdirCheck {
    /// Not this harness's filesystem to judge. The engine falls back to
    /// checking the directory on its own process filesystem, which is the
    /// behavior every host had before this method existed.
    Unmanaged,
    /// The directory exists in that workspace, and this is its canonical path
    /// on the filesystem the agent will actually run on. The engine passes it
    /// to the harness verbatim.
    Resolved(String),
    /// The directory is refused, for this reason. The engine fails the step
    /// with the message, prefixed with the node it came from.
    Refused(String),
}

/// Runs a host-registered, multi-turn **agent** — the harness seam.
///
/// Where [`LlmProvider::complete`](crate::caps::LlmProvider::complete) is a
/// single chat call, an `AgentRunner` runs a *named agent kind* the host has
/// defined — a coding agent, a researcher, a crypto agent — each bringing its
/// own curated tool access, model, sandbox, and iteration policy. An `agent`
/// node selects one via a trusted `agent_ref` in its config; when the host wires
/// this capability, the node runs that agent to completion instead of issuing a
/// bare completion.
///
/// The engine stays host-agnostic: `agent_ref` is an opaque id the host resolves
/// against its own agent registry (or that the graph's own
/// [`agents`](crate::model::WorkflowGraph::agents) registry answers first). This
/// capability is **optional** — hosts without an agent registry leave
/// [`Capabilities::agent`](crate::caps::Capabilities::agent) `None`, and `agent`
/// nodes fall back to [`LlmProvider`](crate::caps::LlmProvider).
///
/// # Implementing it
///
/// Only [`run_agent`](Self::run_agent) is required, and it is unchanged from the
/// previous release. Every other method has a default that degrades to the
/// behavior a host had before richer agents existed, so an existing
/// implementation keeps compiling and keeps behaving identically. Override them
/// in roughly this order of value:
///
/// 1. [`run`](Self::run) — take the typed request, and report a real
///    [`StopReason`]. The default reports [`Finished`](StopReason::Finished) for
///    every run, which is exactly the conflation this trait exists to remove, so
///    a harness with a real loop should not leave it defaulted.
/// 2. [`resolve_agent`](Self::resolve_agent) — expose the host's own agent
///    catalogue to graphs.
/// 3. [`resolve_context`](Self::resolve_context) — expand `host` context
///    sources.
/// 4. [`resolve_tools`](Self::resolve_tools) — describe tools and expand
///    namespace patterns.
#[async_trait]
pub trait AgentRunner: Send + Sync {
    /// Runs the host-registered agent identified by `agent_ref` to completion.
    ///
    /// `request` is the (expression-resolved) node config — prompt/input plus
    /// any per-node overrides the host chooses to honor. `conn` is the same
    /// opaque, host-managed connection reference the other capabilities take.
    /// The returned JSON is treated exactly like a completion response by the
    /// `agent` node (enveloped into `{ json, text, raw }`), so downstream
    /// binding is identical to a plain agent turn.
    ///
    /// # Errors
    /// Returns an [`EngineError::Capability`](crate::error::EngineError::Capability)
    /// when `agent_ref` is unknown or the agent run fails.
    async fn run_agent(&self, agent_ref: &str, request: Value, conn: Option<&str>)
    -> Result<Value>;

    /// Runs a fully-assembled [`AgentRunRequest`] until the loop finishes, hits
    /// a limit, or pauses.
    ///
    /// A limit stop and a pause are **not errors** — they are reported through
    /// [`AgentRunOutcome::stop`], so a harness's own failures (an unreachable
    /// model, a rejected credential) stay distinguishable from a loop that ran
    /// and deliberately stopped.
    ///
    /// The default implementation forwards
    /// [`request.config`](AgentRunRequest::config) — the node's resolved config,
    /// verbatim — to [`run_agent`](Self::run_agent) and reports the result as
    /// [`Finished`](StopReason::Finished). That is byte-for-byte the call a host
    /// received before this method existed, so leaving it defaulted preserves
    /// existing behavior exactly. It does mean the host cannot signal a partial
    /// or paused run, which is why a harness with a real loop should override
    /// it.
    ///
    /// # Errors
    /// Returns an [`EngineError::Capability`](crate::error::EngineError::Capability)
    /// when the harness cannot run the request at all.
    async fn run(&self, request: AgentRunRequest) -> Result<AgentRunOutcome> {
        let value = self
            .run_agent(
                &request.agent.id,
                request.config.clone(),
                request.connection_ref.as_deref(),
            )
            .await?;
        Ok(AgentRunOutcome::finished(value))
    }

    /// Resolves `agent_ref` against the host's own agent catalogue.
    ///
    /// The **fallback** half of agent-kind resolution: a graph's own
    /// [`agents`](crate::model::WorkflowGraph::agents) registry is consulted
    /// first and wins on a collision, so a workflow stays portable between
    /// hosts. This method is how a harness adds curated kinds no graph should
    /// have to inline.
    ///
    /// Read-only on purpose. A directory the engine could write to would let a
    /// running workflow mint itself an agent with tools it was never granted.
    ///
    /// **`Ok(None)` for an unknown ref is a normal outcome, not a soft error** —
    /// the node then passes the ref through to [`run`](Self::run) as a bare
    /// `AgentDefinition { id: agent_ref, .. }`, which is what a harness that
    /// resolves refs internally wants. The default returns `Ok(None)` for
    /// everything, which is exactly that pass-through behavior.
    ///
    /// # Errors
    /// Returns an [`EngineError::Capability`](crate::error::EngineError::Capability)
    /// only on a genuine failure to answer — a broken store, a lost connection.
    async fn resolve_agent(&self, agent_ref: &str) -> Result<Option<AgentDefinition>> {
        let _ = agent_ref;
        Ok(None)
    }

    /// Every agent kind the harness offers, for an editor's picker and for
    /// authoring surfaces. Order should be stable across calls.
    ///
    /// Not on the run path — nothing in [`crate::engine`] calls this.
    ///
    /// # Errors
    /// Returns an [`EngineError::Capability`](crate::error::EngineError::Capability)
    /// when the catalogue cannot be listed.
    async fn list_agents(&self) -> Result<Vec<AgentDefinition>> {
        Ok(Vec::new())
    }

    /// Resolves an opaque
    /// [`ContextSourceKind::Host`](crate::model::ContextSourceKind::Host)
    /// source into a [`ContextBlock`] — identity/soul text, a description of the
    /// integrations this user has connected, a RAG index, learned context.
    ///
    /// This is deliberately *not* a prompt composer. The harness owns the loop
    /// and therefore owns the system prompt; a second prompt-composition seam
    /// here would duplicate or fight the harness's own. All this does is turn an
    /// author-declared source name into a block that rides along in the request.
    ///
    /// It also does not overlap [`MemoryProvider`](crate::caps::MemoryProvider):
    /// `memory` and `flavour` context sources route to that existing capability
    /// unchanged, so the memory `scope` contract keeps exactly one home.
    ///
    /// **`Ok(None)` for an unrecognized `source` is a normal outcome**; the node
    /// then applies the block's `optional` policy, failing unless the author
    /// marked the source optional. Returning `Ok(Some(_))` with an empty block
    /// is different and also normal: the source resolved and has nothing to
    /// contribute this run.
    ///
    /// # Errors
    /// Returns an [`EngineError::Capability`](crate::error::EngineError::Capability)
    /// only on a genuine failure to answer.
    async fn resolve_context(
        &self,
        source: &str,
        params: &Value,
        identity: &AgentRunIdentity,
    ) -> Result<Option<ContextBlock>> {
        let _ = (source, params, identity);
        Ok(None)
    }

    /// Expands an agent's [`ToolGrant`]s into concrete [`ToolDescriptor`]s.
    ///
    /// The discovery half [`ToolInvoker`](crate::caps::ToolInvoker) lacks:
    /// `invoke(slug, args, conn)` can execute a tool but cannot say which tools
    /// exist, what they take, or what `"github.*"` expands to.
    ///
    /// `conn` is the node's connection reference, so a harness whose visible
    /// tool set depends on the account can narrow accordingly. A grant matching
    /// nothing contributes nothing — that is not an error, for the same
    /// build-variance reason an unknown agent id is not.
    ///
    /// The default returns one undescribed [`ToolDescriptor`] per grant, with
    /// patterns forwarded verbatim — appropriate for a harness that already
    /// knows its own tools and only needs to be told which ones are permitted.
    ///
    /// # Errors
    /// Returns an [`EngineError::Capability`](crate::error::EngineError::Capability)
    /// when the catalogue cannot be consulted.
    async fn resolve_tools(
        &self,
        grants: &[ToolGrant],
        conn: Option<&str>,
    ) -> Result<Vec<ToolDescriptor>> {
        Ok(grants
            .iter()
            .map(|grant| ToolDescriptor::from_grant(grant, conn))
            .collect())
    }

    /// Resolves a node's declared working directory (`config.cwd`, a
    /// `sub_workflow` node's `config.workspace`) against `workspace`, on the
    /// filesystem the agent will actually run on.
    ///
    /// The engine has no filesystem of its own. It can, and does, check the
    /// *shape* of a declared directory — an absolute path, a `..` traversal —
    /// because that is string arithmetic. Deciding whether the path **exists**,
    /// what it canonicalizes to, and whether it is a directory is an
    /// outside-world effect, and on a harness whose agents run in a remote
    /// sandbox or a container the answer is not on the engine's disk at all.
    /// This method is where such a host answers for its own filesystem.
    ///
    /// `workspace` is the run's declared boundary and `declared` the raw value
    /// the author wrote (already expression-resolved). A host that resolves the
    /// pair must also **contain** it: returning
    /// [`Resolved`](WorkdirCheck::Resolved) asserts the path is inside
    /// `workspace`, since only the host can compare two paths on its own
    /// filesystem.
    ///
    /// The default returns [`Unmanaged`](WorkdirCheck::Unmanaged) for
    /// everything, so the engine canonicalizes locally exactly as it did before
    /// this method existed. A host whose agents share the engine's filesystem
    /// wants precisely that and should leave it alone.
    async fn resolve_workdir(&self, workspace: &str, declared: &str) -> WorkdirCheck {
        let _ = (workspace, declared);
        WorkdirCheck::Unmanaged
    }
}
