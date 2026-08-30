use super::*;

/// The graph's agent registry and every `agent` node's agent configuration.
///
/// Two things are checked here that cannot be checked anywhere else:
///
/// - **Literal-only reference fields.** `agent_ref`, a tool `slug`, a tool
///   `connection_ref`, and a memory context `scope` may not be
///   `=`-expressions. An expression resolves from run data — which may include
///   model output — so allowing one would let upstream data choose which
///   credential a call acts as, which tool it reaches, or which agent type (and
///   therefore which tool grants) a turn runs with. Rejecting them structurally,
///   before a run starts, is what makes that unbypassable.
/// - **Context node ancestry.** A `node` context source naming a node the agent
///   cannot be reached from would resolve to nothing at run time, and the agent
///   would reason over an empty block without any error. This is statically
///   decidable, so it is caught here.
///
/// An `agent_ref` the graph's registry does not declare is deliberately **not**
/// an error: validation runs without capabilities, and the harness's own
/// registry is the documented fallback. Hosts that want author-time resolution
/// call [`unresolved_agent_refs`].
pub(super) fn validate_agents(graph: &WorkflowGraph, errors: &mut Vec<ValidationError>) {
    use crate::model::{AgentLimits, ContextSource, ContextSourceKind, ToolGrant};

    /// Whether an authored string is an `=`-expression rather than a literal.
    fn is_expression(value: &str) -> bool {
        value.starts_with('=')
    }

    let mut seen_agents = HashSet::new();
    for agent in &graph.agents {
        if agent.id.is_empty() {
            errors.push(ValidationError::InvalidAgentDefinition {
                agent: String::new(),
                reason: "agent definition requires a non-empty `id`".to_string(),
            });
        } else if !seen_agents.insert(agent.id.as_str()) {
            errors.push(ValidationError::DuplicateAgentId(agent.id.clone()));
        }

        for grant in &agent.tools {
            if let Some(reason) = tool_grant_problem(grant) {
                errors.push(ValidationError::InvalidAgentDefinition {
                    agent: agent.id.clone(),
                    reason,
                });
            }
        }
        for (index, source) in agent.context.iter().enumerate() {
            if let Some(reason) = context_source_problem(source, index) {
                errors.push(ValidationError::InvalidAgentDefinition {
                    agent: agent.id.clone(),
                    reason,
                });
            }
        }
        if let Some(reason) = limits_problem(&agent.limits) {
            errors.push(ValidationError::InvalidAgentDefinition {
                agent: agent.id.clone(),
                reason,
            });
        }
    }

    /// Why a tool grant is unacceptable, or `None`.
    fn tool_grant_problem(grant: &ToolGrant) -> Option<String> {
        if grant.slug.is_empty() {
            return Some("tool grant requires a non-empty `slug`".to_string());
        }
        if is_expression(&grant.slug) {
            return Some(format!(
                "tool grant `slug` must be a literal, not the expression {:?} — a tool chosen \
                 by run data is a tool chosen by whatever wrote that data",
                grant.slug
            ));
        }
        if grant.slug == "*" || (grant.slug.contains('*') && !grant.slug.ends_with(".*")) {
            return Some(format!(
                "tool grant slug {:?} is not a valid pattern — only a trailing `.*` on a \
                 non-empty prefix is allowed (e.g. \"github.*\"); to use the harness's default \
                 tools, omit `tools` entirely",
                grant.slug
            ));
        }
        match grant.connection_ref.as_deref() {
            Some(conn) if is_expression(conn) => Some(format!(
                "tool grant `connection_ref` must be a literal, not the expression {conn:?} — a \
                 credential selected by run data is the prompt-injection shape this field exists \
                 to prevent"
            )),
            _ => None,
        }
    }

    /// Why a context source is unacceptable, ignoring graph-position checks
    /// (which only apply to a node, not to a reusable definition).
    fn context_source_problem(source: &ContextSource, index: usize) -> Option<String> {
        let label = source.label_or_index(index);
        match &source.kind {
            ContextSourceKind::Text { text }
                if text.is_null() || text.as_str().is_some_and(str::is_empty) =>
            {
                Some(format!("context source `{label}` has empty `text`"))
            }
            ContextSourceKind::Memory { scope, query, .. } => {
                if !matches!(scope.as_str(), "user" | "flow" | "flows") {
                    // Literal enum, for the same unbypassable reason the `memory`
                    // node validates its scope this way: an `=`-expression is
                    // never one of the three literals, so a run-time-resolved
                    // scope can never reach the memory provider.
                    Some(format!(
                        "context source `{label}` has unknown memory scope {scope:?} \
                         (expected a literal user|flow|flows)"
                    ))
                } else if query.is_empty() {
                    Some(format!(
                        "context source `{label}` has an empty memory `query`"
                    ))
                } else {
                    None
                }
            }
            ContextSourceKind::Flavour { slug } if slug.is_empty() => Some(format!(
                "context source `{label}` has an empty flavour `slug`"
            )),
            ContextSourceKind::Host { source: name, .. } => {
                if name.is_empty() {
                    Some(format!(
                        "context source `{label}` has an empty host `source`"
                    ))
                } else if is_expression(name) {
                    Some(format!(
                        "context source `{label}` host `source` must be a literal, not the \
                         expression {name:?} — which corpus an agent reads from should not be \
                         chosen by run data"
                    ))
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Why a limits block is unacceptable, or `None`.
    fn limits_problem(limits: &AgentLimits) -> Option<String> {
        let zero = [
            ("max_steps", limits.max_steps),
            ("max_tool_calls", limits.max_tool_calls),
            ("agent_timeout_secs", limits.agent_timeout_secs),
            ("tool_timeout_secs", limits.tool_timeout_secs),
        ]
        .into_iter()
        .find(|(_, v)| *v == Some(0));
        zero.map(|(field, _)| {
            format!(
                "`limits.{field}` must be greater than 0 (an agent that may take 0 steps \
                     cannot run); omit it for no bound"
            )
        })
    }

    for node in &graph.nodes {
        if node.kind != NodeKind::Agent {
            continue;
        }
        let config = &node.config;

        match config.get("agent_ref") {
            Some(Value::String(agent_ref)) if is_expression(agent_ref) => {
                errors.push(ValidationError::InvalidNodeConfig {
                    node: node.id.clone(),
                    reason: format!(
                        "`agent_ref` must be a literal, not the expression {agent_ref:?} — an \
                         expression resolves from run data, which would let upstream (possibly \
                         model-influenced) data select a differently-privileged agent type"
                    ),
                });
            }
            Some(value) if !value.is_string() && !value.is_null() => {
                errors.push(ValidationError::InvalidNodeConfig {
                    node: node.id.clone(),
                    reason: "`agent_ref` must be a string".to_string(),
                });
            }
            _ => {}
        }

        // Tool grants. Parsed rather than hand-walked so an author gets the same
        // shape errors the executor would raise, before a run starts.
        match config.get("tools") {
            Some(raw) if !raw.is_null() => {
                match serde_json::from_value::<Vec<ToolGrant>>(raw.clone()) {
                    Ok(grants) => {
                        for grant in &grants {
                            if let Some(reason) = tool_grant_problem(grant) {
                                errors.push(ValidationError::InvalidNodeConfig {
                                    node: node.id.clone(),
                                    reason,
                                });
                            }
                        }
                        // A node may narrow its agent definition's grants, never
                        // widen them. Only checkable when the definition is
                        // in-graph; a harness-resolved one is dropped with a
                        // warning at run time instead.
                        if let Some(agent_ref) = config.get("agent_ref").and_then(Value::as_str)
                            && let Some(definition) = graph.agent(agent_ref)
                            && !definition.tools.is_empty()
                        {
                            for grant in &grants {
                                if !definition
                                    .tools
                                    .iter()
                                    .any(|g| g.covers(&grant.slug) || grant.covers(&g.slug))
                                {
                                    errors.push(ValidationError::InvalidNodeConfig {
                                        node: node.id.clone(),
                                        reason: format!(
                                            "tool {:?} is not granted by agent {agent_ref:?} — a \
                                             node may narrow its agent's tools, never widen them",
                                            grant.slug
                                        ),
                                    });
                                }
                            }
                        }
                    }
                    Err(e) => errors.push(ValidationError::InvalidNodeConfig {
                        node: node.id.clone(),
                        reason: format!("invalid `tools`: {e}"),
                    }),
                }
            }
            _ => {}
        }

        match config.get("context") {
            Some(raw) if !raw.is_null() => {
                match serde_json::from_value::<Vec<ContextSource>>(raw.clone()) {
                    Ok(sources) => {
                        for (index, source) in sources.iter().enumerate() {
                            if let Some(reason) = context_source_problem(source, index) {
                                errors.push(ValidationError::InvalidNodeConfig {
                                    node: node.id.clone(),
                                    reason,
                                });
                            }
                        }
                    }
                    Err(e) => errors.push(ValidationError::InvalidNodeConfig {
                        node: node.id.clone(),
                        reason: format!("invalid `context`: {e}"),
                    }),
                }
            }
            _ => {}
        }

        match config.get("limits") {
            Some(raw) if !raw.is_null() => match serde_json::from_value::<AgentLimits>(raw.clone())
            {
                Ok(limits) => {
                    if let Some(reason) = limits_problem(&limits) {
                        errors.push(ValidationError::InvalidNodeConfig {
                            node: node.id.clone(),
                            reason,
                        });
                    }
                }
                Err(e) => errors.push(ValidationError::InvalidNodeConfig {
                    node: node.id.clone(),
                    reason: format!("invalid `limits`: {e}"),
                }),
            },
            _ => {}
        }
    }
}

/// Every (node id, `agent_ref`) pair the graph's own registry does not resolve.
///
/// [`validate_all`] deliberately does not treat these as errors: it runs without
/// capabilities, and the harness's registry is the documented fallback — a graph
/// naming a host-curated agent is valid, not broken. A host that *can* consult
/// its registry at author time calls this and checks each ref against its own
/// catalogue, turning "will fail at run time" into an editor warning.
///
/// ```
/// use tinyflows::model::WorkflowGraph;
/// use tinyflows::validate::unresolved_agent_refs;
///
/// let graph: WorkflowGraph = serde_json::from_str(
///     r#"{
///       "agents": [{"id": "known"}],
///       "nodes": [
///         {"id": "a", "kind": "agent", "name": "a", "config": {"agent_ref": "known"}},
///         {"id": "b", "kind": "agent", "name": "b", "config": {"agent_ref": "host_side"}}
///       ],
///       "edges": []
///     }"#,
/// )
/// .unwrap();
///
/// assert_eq!(
///     unresolved_agent_refs(&graph),
///     vec![("b".to_string(), "host_side".to_string())]
/// );
/// ```
#[must_use]
pub fn unresolved_agent_refs(graph: &WorkflowGraph) -> Vec<(crate::model::NodeId, String)> {
    graph
        .nodes
        .iter()
        .filter(|node| node.kind == NodeKind::Agent)
        .filter_map(|node| {
            let agent_ref = node.config.get("agent_ref").and_then(Value::as_str)?;
            (!agent_ref.is_empty() && graph.agent(agent_ref).is_none())
                .then(|| (node.id.clone(), agent_ref.to_string()))
        })
        .collect()
}
