use super::*;
use serde_json::json;

#[test]
fn a_definition_needs_only_an_id() {
    let agent: AgentDefinition = serde_json::from_value(json!({ "id": "researcher" })).unwrap();
    assert_eq!(agent, AgentDefinition::new("researcher"));
}

#[test]
fn a_full_definition_round_trips() {
    let wire = json!({
        "id": "triager",
        "name": "Issue triager",
        "description": "Labels and routes inbound issues.",
        "instructions": "Be terse.",
        "model": "claude-opus-5",
        "provider": "anthropic",
        "limits": { "max_steps": 8, "max_tool_calls": 20 },
        "tools": [
            { "slug": "github.search_issues" },
            { "slug": "github.add_labels", "connection_ref": "conn_gh_bot" }
        ],
        "context": [
            { "kind": "memory", "scope": "user", "query": "triage preferences", "limit": 5 },
            { "kind": "host", "source": "repo_conventions", "params": { "repo": "=inputs.repo" } }
        ],
        "metadata": { "tier": "fast" }
    });
    let agent: AgentDefinition = serde_json::from_value(wire.clone()).unwrap();

    assert_eq!(agent.provider.as_deref(), Some("anthropic"));
    assert_eq!(agent.limits.max_steps, Some(8));
    assert_eq!(agent.tools.len(), 2);
    assert_eq!(agent.context[0].kind.as_str(), "memory");
    assert_eq!(agent.metadata.get("tier"), Some(&json!("fast")));

    // Optional/absent fields are elided, so a round trip is byte-stable.
    assert_eq!(serde_json::to_value(&agent).unwrap(), wire);
}

#[test]
fn todays_tool_descriptor_shape_still_deserializes() {
    // The `{slug, connection_ref}` objects existing `agent` node configs
    // already carry must load as grants unchanged.
    let grants: Vec<ToolGrant> = serde_json::from_value(json!([
        { "slug": "SOME_TOOL_ACTION" },
        { "slug": "OTHER_ACTION", "connection_ref": "acct_9" }
    ]))
    .unwrap();
    assert_eq!(grants[0], ToolGrant::new("SOME_TOOL_ACTION"));
    assert_eq!(grants[1].connection_ref.as_deref(), Some("acct_9"));
}

#[test]
fn a_context_source_flattens_its_kind() {
    let source: ContextSource =
        serde_json::from_value(json!({ "kind": "text", "text": "hello", "label": "Greeting" }))
            .unwrap();
    assert_eq!(source.label.as_deref(), Some("Greeting"));
    assert!(!source.optional, "sources are required by default");
    assert_eq!(
        source.kind,
        ContextSourceKind::Text {
            text: Value::from("hello")
        }
    );
}

#[test]
fn a_unit_kind_needs_no_payload() {
    let source: ContextSource = serde_json::from_value(json!({ "kind": "items" })).unwrap();
    assert_eq!(source.kind, ContextSourceKind::Items);
    assert_eq!(source.label_or_index(2), "context_2");
}

#[test]
fn limits_narrow_but_never_widen() {
    let definition = AgentLimits {
        max_steps: Some(8),
        max_tool_calls: Some(20),
        agent_timeout_secs: None,
        tool_timeout_secs: Some(60),
    };
    let node = AgentLimits {
        max_steps: Some(4),
        max_tool_calls: Some(99),
        agent_timeout_secs: Some(30),
        tool_timeout_secs: Some(90),
    };
    assert_eq!(
        definition.narrowed_by(&node),
        AgentLimits {
            max_steps: Some(4),
            max_tool_calls: Some(20),
            agent_timeout_secs: Some(30),
            tool_timeout_secs: Some(60),
        }
    );
    assert!(AgentLimits::default().is_empty());
}

#[test]
fn a_pattern_grant_covers_its_namespace_only() {
    let grant = ToolGrant::new("github.*");
    assert!(grant.is_pattern());
    assert!(grant.covers("github.add_labels"));
    assert!(!grant.covers("gitlab.add_labels"));
    assert!(!ToolGrant::new("github.add_labels").covers("github.search"));
}
