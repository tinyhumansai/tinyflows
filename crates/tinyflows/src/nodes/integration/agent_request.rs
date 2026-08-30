//! Assembling an [`AgentRunRequest`] from an `agent` node's declarative config.
//!
//! This is the whole of tinyflows' contribution to running an agent: the
//! model↔tool loop belongs to the harness, so the engine's job is to turn what
//! the author *declared* into one inert, fully-resolved request.
//!
//! ```text
//!   graph `agents` registry ──┐
//!   AgentRunner::resolve_agent ┼─► definition ─► merge node overrides
//!   pass-through {id}         ─┘                        │
//!                                                       ▼
//!                        context sources ─► ContextBlock[]  (engine + harness)
//!                        tool grants     ─► ToolDescriptor[] (harness)
//!                                                       │
//!                                                       ▼
//!                                              AgentRunRequest
//! ```
//!
//! ## Narrowing, never widening
//!
//! A node may reference a curated agent type and tighten it — fewer tools,
//! lower limits, extra instructions — but never loosen it. An author who could
//! widen a definition from the node that uses it would make the definition
//! worthless as a statement of what an agent is allowed to do. See
//! [`merge_node_overrides`].

use serde_json::{Map, Value};

use crate::caps::{
    AgentInput, AgentModelSelection, AgentRunIdentity, AgentRunRequest, ContextBlock,
    ToolDescriptor,
};
use crate::error::{EngineError, Result};
use crate::model::{AgentDefinition, AgentLimits, ContextSource, ContextSourceKind, ToolGrant};
use crate::nodes::NodeContext;

/// Reads a node config's `agent_ref`, ignoring an empty string.
///
/// Empty is treated as absent so `{"agent_ref": ""}` keeps falling back to a
/// plain completion rather than asking the harness for an agent called `""`.
pub(crate) fn agent_ref_of(cfg: &Value) -> Option<&str> {
    cfg.get("agent_ref")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// Deserializes a node config key, reporting a bad shape as a capability error
/// naming the node and the key rather than a bare serde message.
fn field<T: serde::de::DeserializeOwned + Default>(
    cfg: &Value,
    key: &str,
    node_id: &str,
) -> Result<T> {
    match cfg.get(key) {
        None | Some(Value::Null) => Ok(T::default()),
        Some(value) => serde_json::from_value(value.clone()).map_err(|e| {
            EngineError::Capability(format!("agent node {node_id}: invalid `{key}`: {e}"))
        }),
    }
}

/// Resolves `agent_ref` to a definition: the graph's own registry first, then
/// the harness's, then a bare pass-through.
///
/// The order is the portability contract. A workflow that carries its own
/// definition behaves identically on every host, so an in-graph entry always
/// wins. Only when the graph is silent does the harness's curated catalogue
/// answer, and a ref neither knows is **not** an error — it becomes
/// `AgentDefinition { id: agent_ref, .. }`, which is exactly what a harness that
/// resolves refs internally expects (and what it received before the registry
/// existed).
pub(crate) async fn resolve_definition(
    ctx: &NodeContext<'_>,
    agent_ref: &str,
) -> Result<AgentDefinition> {
    if let Some(in_graph) = ctx.agents.iter().find(|a| a.id == agent_ref) {
        tracing::debug!(agent_ref, "agent node: resolved from the graph registry");
        return Ok(in_graph.clone());
    }
    if let Some(runner) = ctx.caps.agent.as_ref()
        && let Some(from_host) = runner.resolve_agent(agent_ref).await?
    {
        tracing::debug!(agent_ref, "agent node: resolved from the host registry");
        return Ok(from_host);
    }
    tracing::debug!(
        agent_ref,
        "agent node: no registry entry; passing the ref through to the harness"
    );
    Ok(AgentDefinition::new(agent_ref))
}

/// Applies a node's overrides to its agent definition, narrowing only.
///
/// | field | rule |
/// |---|---|
/// | `instructions` | the node's are **appended** to the definition's, never replacing them — a node must not be able to silently neuter a curated agent's standing instructions |
/// | `model`, `provider`, `cwd`/`working_dir` | the node's win when set |
/// | `context` | the definition's blocks first, then the node's |
/// | `tools` | when the definition grants any, the node's list **intersects** them and may not add a tool the definition never granted; when the definition grants none there is nothing to narrow against, so the node's stand |
/// | `limits` | field-wise, keeping the **lower** of each declared bound |
/// | `metadata` | shallow key merge, the node's winning per key |
///
/// A node tool that the definition never granted is dropped with a warning
/// rather than failing the run: [`crate::validate`] already rejects that
/// statically for an in-graph definition, so reaching here means the definition
/// came from the harness and the graph could not have known.
pub(crate) fn merge_node_overrides(
    mut definition: AgentDefinition,
    cfg: &Value,
    node_id: &str,
) -> Result<AgentDefinition> {
    if let Some(extra) = cfg.get("instructions").and_then(Value::as_str)
        && !extra.is_empty()
    {
        definition.instructions = Some(match definition.instructions {
            Some(base) if !base.is_empty() => format!("{base}\n\n{extra}"),
            _ => extra.to_string(),
        });
    }

    if let Some(model) = cfg.get("model").and_then(Value::as_str) {
        definition.model = Some(model.to_string());
    }
    if let Some(provider) = cfg.get("provider").and_then(Value::as_str) {
        definition.provider = Some(provider.to_string());
    }
    if let Some(working_dir) = declared_working_dir(cfg, node_id)? {
        definition.working_dir = Some(working_dir);
    }

    let node_context: Vec<ContextSource> = field(cfg, "context", node_id)?;
    definition.context.extend(node_context);

    let node_tools: Vec<ToolGrant> = field(cfg, "tools", node_id)?;
    if !node_tools.is_empty() {
        definition.tools = if definition.tools.is_empty() {
            node_tools
        } else {
            narrow_tools(&definition.tools, &node_tools, node_id)
        };
    }

    let node_limits: AgentLimits = field(cfg, "limits", node_id)?;
    definition.limits = definition.limits.narrowed_by(&node_limits);

    let node_metadata: Map<String, Value> = field(cfg, "metadata", node_id)?;
    definition.metadata.extend(node_metadata);

    Ok(definition)
}

/// Intersects a node's tool list with its definition's grants.
///
/// A definition grant survives when the node names it — exactly, or through a
/// pattern the node wrote (`"github.*"` keeps every `github.` grant). The
/// definition's descriptor is what survives, so its trusted `connection_ref`
/// wins over any the node supplied: the definition is the more reviewed
/// artifact, and a per-node grant must not be able to repoint a curated tool at
/// a different credential.
fn narrow_tools(granted: &[ToolGrant], requested: &[ToolGrant], node_id: &str) -> Vec<ToolGrant> {
    for want in requested {
        if !granted
            .iter()
            .any(|g| g.covers(&want.slug) || want.covers(&g.slug))
        {
            tracing::warn!(
                node = node_id,
                slug = %want.slug,
                "agent node: tool is not granted by the agent definition; ignoring"
            );
        }
    }
    granted
        .iter()
        .filter(|g| {
            requested
                .iter()
                .any(|w| w.covers(&g.slug) || g.covers(&w.slug))
        })
        .cloned()
        .collect()
}

/// The working directory a node declared, under either spelling.
///
/// `cwd` is the name every other surface in the crate uses for "run this step
/// over there" — a `shell` node's `config.cwd`, a script step's `args.cwd` — so
/// it is the one to reach for. `working_dir` is the older spelling, and the name
/// of the field on [`AgentDefinition`], so it stays accepted; `cwd` wins when
/// both are set.
///
/// # Errors
/// Refuses a non-string, a blank value, **or a null**, exactly as a `shell`
/// node's `cwd` does. A number or an empty string here is an authoring slip,
/// and the alternative is a step that silently runs somewhere else.
///
/// Null is the one worth spelling out. `cfg` arrives already
/// expression-resolved, so `"cwd": "=nodes.prepare.item.json.worktree"` becomes
/// `null` whenever that path is missing — the upstream node failed, or the key
/// moved. Treating that as "no `cwd` declared" would fall through to
/// `working_dir`, then to the definition's own directory, then to whatever the
/// harness defaults to: the step runs in a *different checkout* and says
/// nothing. A directory an author named is never silently swapped for another,
/// so a present-but-null value fails the node instead.
pub(crate) fn declared_working_dir(cfg: &Value, node_id: &str) -> Result<Option<String>> {
    for key in ["cwd", "working_dir"] {
        let Some(value) = cfg.get(key) else {
            continue;
        };
        if value.is_null() {
            return Err(EngineError::Capability(format!(
                "agent node {node_id}: `{key}` resolved to null; an expression that reads a \
                 missing path fails the step rather than falling back to another directory"
            )));
        }
        let dir = value.as_str().ok_or_else(|| {
            EngineError::Capability(format!("agent node {node_id}: `{key}` must be a string"))
        })?;
        if dir.trim().is_empty() {
            return Err(EngineError::Capability(format!(
                "agent node {node_id}: `{key}` must be a non-empty path when present"
            )));
        }
        return Ok(Some(dir.to_string()));
    }
    Ok(None)
}

/// Resolves a declared working directory against the run's workspace.
///
/// The value is already expression-resolved by the node's config pass, which is
/// the point: the directory an `agent` node runs in is usually one an earlier
/// node just created, addressed as `"=nodes.prepare.item.json.worktree"`.
///
/// On a run with no workspace the string passes through untouched — see
/// [`crate::workdir`] for why the engine will not check a filesystem the agent
/// may not even be running on.
///
/// # Errors
/// Returns [`EngineError::Capability`] when the directory escapes the
/// workspace, does not exist, or is not a directory.
pub(crate) async fn resolve_working_dir(
    ctx: &NodeContext<'_>,
    raw: &str,
    key: &str,
) -> Result<String> {
    crate::workdir::resolve_node_dir(
        ctx.caps.agent.as_ref(),
        ctx.run,
        raw,
        &format!("config.{key}"),
        &format!("agent node {}", ctx.node.id),
    )
    .await
}

/// Resolves each declared [`ContextSource`] into a [`ContextBlock`], in
/// declaration order.
///
/// The engine resolves four kinds itself and delegates the fifth:
///
/// - `text` — already expression-resolved by the node's config pass; wrapped.
/// - `items` — the node's input items.
/// - `memory` / `flavour` — the existing
///   [`MemoryProvider`](crate::caps::MemoryProvider), so the memory `scope`
///   contract keeps exactly one home.
/// - `host` — [`AgentRunner::resolve_context`](crate::caps::AgentRunner::resolve_context).
///
/// A source that cannot be resolved — the capability is not wired, or the
/// harness does not recognize it — **fails the node** unless the author set
/// `optional: true`. Silently dropping context is the wrong default: an agent
/// missing its identity text or its connected-integration list still answers,
/// confidently and wrongly, and that failure passes a smoke test.
pub(crate) async fn resolve_context(
    ctx: &NodeContext<'_>,
    sources: &[ContextSource],
    identity: &AgentRunIdentity,
) -> Result<Vec<ContextBlock>> {
    let mut blocks = Vec::with_capacity(sources.len());
    for (index, source) in sources.iter().enumerate() {
        let label = source.label_or_index(index);
        let kind = source.kind.as_str();

        let resolved: Option<ContextBlock> = match &source.kind {
            ContextSourceKind::Text { text } => Some(match text {
                Value::String(s) => ContextBlock::text(&label, kind, s),
                Value::Null => ContextBlock::text(&label, kind, ""),
                other => ContextBlock {
                    label: label.clone(),
                    source_kind: kind.to_string(),
                    text: Some(other.to_string()),
                    data: other.clone(),
                },
            }),
            ContextSourceKind::Items => Some(ContextBlock::data(
                &label,
                kind,
                Value::Array(ctx.input.iter().map(|i| i.json.clone()).collect()),
            )),
            ContextSourceKind::Memory {
                scope,
                query,
                limit,
            } => match ctx.caps.memory.as_ref() {
                Some(memory) => {
                    let mut opts = Map::new();
                    opts.insert("operation".to_string(), Value::from("recall"));
                    if let Some(limit) = limit {
                        opts.insert("limit".to_string(), Value::from(*limit));
                    }
                    let data = memory.recall(scope, query, Value::Object(opts)).await?;
                    Some(ContextBlock::data(&label, kind, data))
                }
                None => None,
            },
            ContextSourceKind::Flavour { slug } => match ctx.caps.memory.as_ref() {
                Some(memory) => Some(ContextBlock::data(
                    &label,
                    kind,
                    memory.flavour(slug).await?,
                )),
                None => None,
            },
            ContextSourceKind::Host { source, params } => match ctx.caps.agent.as_ref() {
                Some(runner) => runner.resolve_context(source, params, identity).await?,
                None => None,
            },
        };

        match resolved {
            Some(block) => blocks.push(block),
            None if source.optional => {
                tracing::debug!(
                    node = %identity.node_id,
                    label = %label,
                    kind,
                    "agent node: optional context source did not resolve; skipping"
                );
            }
            None => {
                return Err(EngineError::Capability(format!(
                    "agent node {}: context source `{label}` ({kind}) could not be resolved — \
                     the host wired no capability for it, or does not recognize it; \
                     set `\"optional\": true` on the source to make this survivable",
                    identity.node_id
                )));
            }
        }
    }
    Ok(blocks)
}

/// Builds the run identity from the run-state `run` slice.
///
/// Everything here is informational: the engine enforces none of it. It exists
/// because a harness that cannot attribute an agent run to a workflow run cannot
/// bill it, trace it, or cap it.
pub(crate) fn identity_of(ctx: &NodeContext<'_>, item_index: Option<usize>) -> AgentRunIdentity {
    AgentRunIdentity {
        run_id: ctx
            .run
            .get("run_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        workflow_id: ctx
            .run
            .get("workflow_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        node_id: ctx.node.id.clone(),
        depth: ctx
            .run
            .get("sub_workflow_depth")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32,
        item_index,
    }
}

/// Assembles the full request for one agent turn.
///
/// `cfg` is the node's already expression-resolved config; the definition's own
/// `=`-expressions are resolved here, against the same scope, because a
/// definition lives in the graph's `agents` registry and never passes through
/// the node's config resolution.
pub(crate) async fn assemble(
    ctx: &NodeContext<'_>,
    cfg: &Value,
    agent_ref: &str,
    scope: &Value,
    item_index: Option<usize>,
) -> Result<AgentRunRequest> {
    let definition = resolve_definition(ctx, agent_ref).await?;

    // Resolve the definition's `=`-expressions against the node's scope. Done on
    // the definition alone, before merging, so node-side values (already
    // resolved by the node's config pass) are never resolved a second time — a
    // resolved value that happens to start with `=` would otherwise be
    // re-evaluated.
    let definition: AgentDefinition = {
        let raw = serde_json::to_value(&definition)
            .map_err(|e| EngineError::Capability(format!("agent node {}: {e}", ctx.node.id)))?;
        let resolved = crate::expr::resolve_traced(&raw, scope).0;
        serde_json::from_value(resolved).map_err(|e| {
            EngineError::Capability(format!(
                "agent node {}: agent definition `{agent_ref}` did not survive expression \
                 resolution: {e}",
                ctx.node.id
            ))
        })?
    };

    let mut agent = merge_node_overrides(definition, cfg, &ctx.node.id)?;
    // The effective working directory, resolved against the run's workspace and
    // refused if it escapes it. Done on the merged value so a definition's own
    // `working_dir` is held to the same rule as a node's `cwd`.
    if let Some(raw) = agent.working_dir.clone() {
        let key = if cfg.get("cwd").is_some() {
            "cwd"
        } else {
            "working_dir"
        };
        agent.working_dir = Some(resolve_working_dir(ctx, &raw, key).await?);
    }
    let identity = identity_of(ctx, item_index);
    let conn = cfg.get("connection_ref").and_then(Value::as_str);

    let context = resolve_context(ctx, &agent.context, &identity).await?;

    let tools = match ctx.caps.agent.as_ref() {
        Some(runner) => runner.resolve_tools(&agent.tools, conn).await?,
        None => agent
            .tools
            .iter()
            .map(|g| ToolDescriptor::from_grant(g, conn))
            .collect(),
    };

    Ok(AgentRunRequest {
        model: AgentModelSelection {
            model: agent.model.clone(),
            provider: agent.provider.clone(),
        },
        input: AgentInput {
            text: cfg
                .get("prompt")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            data: ctx.input.iter().map(|i| i.json.clone()).collect(),
        },
        context,
        tools,
        connection_ref: conn.map(str::to_string),
        working_dir: agent.working_dir.clone(),
        identity,
        metadata: agent.metadata.clone(),
        output_schema: cfg
            .get("output_parser")
            .and_then(|p| p.get("schema"))
            .filter(|s| !s.is_null())
            .cloned(),
        // Forwarded verbatim so the default `AgentRunner::run` can replay the
        // exact `run_agent(agent_ref, config, conn)` call a host received before
        // this seam existed. Also the escape hatch for any key the typed request
        // does not model.
        config: cfg.clone(),
        agent,
    })
}

#[cfg(test)]
#[path = "agent_request_tests.rs"]
mod tests;
