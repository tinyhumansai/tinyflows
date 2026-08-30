use super::*;
use serde_json::json;

fn definition() -> AgentDefinition {
    AgentDefinition {
        id: "triager".into(),
        instructions: Some("Be terse.".into()),
        model: Some("sonnet".into()),
        provider: Some("anthropic".into()),
        limits: AgentLimits {
            max_steps: Some(8),
            max_tool_calls: Some(20),
            agent_timeout_secs: None,
            tool_timeout_secs: Some(60),
        },
        tools: vec![
            ToolGrant::new("github.search"),
            ToolGrant {
                slug: "github.label".into(),
                connection_ref: Some("conn_definition".into()),
            },
        ],
        metadata: json!({ "tier": "fast", "sandbox": "none" })
            .as_object()
            .unwrap()
            .clone(),
        ..Default::default()
    }
}

fn merged(cfg: Value) -> AgentDefinition {
    merge_node_overrides(definition(), &cfg, "n").expect("merge")
}

#[test]
fn node_instructions_append_rather_than_replace() {
    let agent = merged(json!({ "instructions": "Prefer `bug`." }));
    assert_eq!(
        agent.instructions.as_deref(),
        Some("Be terse.\n\nPrefer `bug`.")
    );
}

#[test]
fn node_model_and_provider_override() {
    let agent = merged(json!({ "model": "opus", "provider": "bedrock" }));
    assert_eq!(agent.model.as_deref(), Some("opus"));
    assert_eq!(agent.provider.as_deref(), Some("bedrock"));

    // An absent override leaves the definition's choice alone.
    let untouched = merged(json!({}));
    assert_eq!(untouched.model.as_deref(), Some("sonnet"));
    assert_eq!(untouched.provider.as_deref(), Some("anthropic"));
}

#[test]
fn node_tools_narrow_and_cannot_widen() {
    let agent = merged(json!({ "tools": [{ "slug": "github.search" }] }));
    assert_eq!(agent.tools.len(), 1);
    assert_eq!(agent.tools[0].slug, "github.search");

    // A tool the definition never granted is dropped, not added.
    let agent = merged(json!({ "tools": [
            { "slug": "github.search" },
            { "slug": "shell.exec" }
        ] }));
    assert_eq!(
        agent
            .tools
            .iter()
            .map(|t| t.slug.as_str())
            .collect::<Vec<_>>(),
        ["github.search"]
    );
}

#[test]
fn the_definitions_tool_connection_wins_over_the_nodes() {
    let agent = merged(json!({ "tools": [
            { "slug": "github.label", "connection_ref": "conn_attacker" }
        ] }));
    assert_eq!(
        agent.tools[0].connection_ref.as_deref(),
        Some("conn_definition"),
        "a node grant must not repoint a curated tool at another credential"
    );
}

#[test]
fn a_node_pattern_narrows_to_the_matching_grants() {
    let agent = merged(json!({ "tools": [{ "slug": "github.*" }] }));
    assert_eq!(agent.tools.len(), 2, "the pattern keeps both github grants");
}

#[test]
fn a_definition_granting_nothing_leaves_node_tools_alone() {
    // Nothing to narrow against, so the node's list stands — this is what
    // keeps a plain `agent` node with a `tools` list working unchanged.
    let bare = AgentDefinition::new("anon");
    let agent =
        merge_node_overrides(bare, &json!({ "tools": [{ "slug": "anything" }] }), "n").unwrap();
    assert_eq!(agent.tools.len(), 1);
}

#[test]
fn node_limits_only_tighten() {
    let agent = merged(json!({ "limits": { "max_steps": 4, "max_tool_calls": 999 } }));
    assert_eq!(agent.limits.max_steps, Some(4));
    assert_eq!(agent.limits.max_tool_calls, Some(20), "widening is ignored");
}

#[test]
fn node_metadata_merges_per_key() {
    let agent = merged(json!({ "metadata": { "tier": "slow", "extra": true } }));
    assert_eq!(agent.metadata.get("tier"), Some(&json!("slow")));
    assert_eq!(agent.metadata.get("sandbox"), Some(&json!("none")));
    assert_eq!(agent.metadata.get("extra"), Some(&json!(true)));
}

#[test]
fn node_context_appends_after_the_definitions() {
    let mut with_context = definition();
    with_context.context = vec![ContextSource::new(ContextSourceKind::Text {
        text: "standing".into(),
    })];
    let agent = merge_node_overrides(
        with_context,
        &json!({ "context": [{ "kind": "items" }] }),
        "n",
    )
    .unwrap();
    assert_eq!(agent.context.len(), 2);
    assert_eq!(agent.context[0].kind.as_str(), "text");
    assert_eq!(agent.context[1].kind.as_str(), "items");
}

#[test]
fn a_malformed_config_key_names_the_node_and_the_key() {
    let err = merge_node_overrides(definition(), &json!({ "limits": "loads" }), "triage")
        .expect_err("should reject");
    let message = err.to_string();
    assert!(message.contains("triage"), "{message}");
    assert!(message.contains("limits"), "{message}");
}

#[test]
fn an_empty_agent_ref_reads_as_absent() {
    assert_eq!(agent_ref_of(&json!({ "agent_ref": "" })), None);
    assert_eq!(agent_ref_of(&json!({})), None);
    assert_eq!(agent_ref_of(&json!({ "agent_ref": "x" })), Some("x"));
}
