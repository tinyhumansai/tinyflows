use super::*;
use serde_json::json;

/// A host written against the previous release: one method, JSON in and out.
struct LegacyRunner;

#[async_trait]
impl AgentRunner for LegacyRunner {
    async fn run_agent(
        &self,
        agent_ref: &str,
        request: Value,
        conn: Option<&str>,
    ) -> Result<Value> {
        Ok(json!({ "agent": agent_ref, "request": request, "connection": conn }))
    }
}

fn request(config: Value) -> AgentRunRequest {
    AgentRunRequest {
        agent: AgentDefinition::new("researcher"),
        model: AgentModelSelection::default(),
        input: AgentInput::default(),
        context: Vec::new(),
        tools: Vec::new(),
        connection_ref: Some("conn_1".to_string()),
        working_dir: None,
        identity: AgentRunIdentity::default(),
        metadata: Map::new(),
        output_schema: None,
        config,
    }
}

#[tokio::test]
async fn the_default_run_forwards_the_config_verbatim_to_run_agent() {
    // The non-breaking guarantee: a host that implements only `run_agent`
    // receives exactly the (agent_ref, config, conn) triple it always did.
    let config = json!({ "prompt": "hi", "agent_ref": "researcher" });
    let outcome = LegacyRunner.run(request(config.clone())).await.unwrap();

    assert_eq!(
        outcome.raw,
        json!({ "agent": "researcher", "request": config, "connection": "conn_1" })
    );
    assert_eq!(outcome.stop, StopReason::Finished);
}

#[tokio::test]
async fn the_default_resolvers_report_absence_not_failure() {
    assert!(
        LegacyRunner
            .resolve_agent("anything")
            .await
            .unwrap()
            .is_none()
    );
    assert!(LegacyRunner.list_agents().await.unwrap().is_empty());
    assert!(
        LegacyRunner
            .resolve_context("soul", &Value::Null, &AgentRunIdentity::default())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn the_default_tool_resolver_passes_grants_through_undescribed() {
    let grants = vec![
        ToolGrant::new("github.*"),
        ToolGrant {
            slug: "slack.post".to_string(),
            connection_ref: Some("acct_slack".to_string()),
        },
    ];
    let tools = LegacyRunner
        .resolve_tools(&grants, Some("node_conn"))
        .await
        .unwrap();

    assert_eq!(tools[0].slug, "github.*", "patterns are forwarded verbatim");
    assert_eq!(
        tools[0].connection_ref.as_deref(),
        Some("node_conn"),
        "a grant without its own connection falls back to the node's"
    );
    assert_eq!(
        tools[1].connection_ref.as_deref(),
        Some("acct_slack"),
        "a grant's own connection wins"
    );
    assert!(tools[0].input_schema.is_none());
}

#[test]
fn finished_derives_text_and_json_like_the_envelope() {
    let from_object = AgentRunOutcome::finished(json!({ "text": "done", "n": 1 }));
    assert_eq!(from_object.text.as_deref(), Some("done"));
    assert_eq!(from_object.json, json!({ "text": "done", "n": 1 }));

    let from_string = AgentRunOutcome::finished(json!("just prose"));
    assert_eq!(from_string.text.as_deref(), Some("just prose"));
    assert_eq!(
        from_string.json,
        Value::Null,
        "a scalar carries no structure"
    );
}

#[test]
fn stop_reasons_have_stable_wire_names() {
    assert_eq!(StopReason::Finished.as_str(), "finished");
    assert_eq!(
        StopReason::LimitStop {
            limit: "max_steps".into()
        }
        .as_str(),
        "limit_stop"
    );
    assert_eq!(
        serde_json::to_value(StopReason::LimitStop {
            limit: "max_steps".into()
        })
        .unwrap(),
        json!({ "stop": "limit_stop", "limit": "max_steps" })
    );
}
