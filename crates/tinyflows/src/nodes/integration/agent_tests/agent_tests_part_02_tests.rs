
#[tokio::test]
async fn plain_agent_without_sub_ports_is_unchanged() {
    // Back-compat: no tools / output_parser configured ⇒ the completion is
    // emitted verbatim (the mock echoes the request under `completion`).
    let node = agent_node(json!({ "prompt": "hi" }));
    let value = run_agent(&node, &mock_capabilities()).await;
    assert_eq!(value["json"]["completion"]["prompt"], "hi");
    assert!(value["json"].get("tool_result").is_none());
}

// ---- configurable agents: registry, merge, context, tools, stop reasons --

mod configurable {
    use super::{agent_node, run_agent};
    use crate::caps::mock::{
        MockAgentHarness, MockLimitedAgentRunner, MockPausingAgentRunner,
        mock_capabilities_with_agent,
    };
    use crate::caps::{AgentRunner, Capabilities};
    use crate::data::Item;
    use crate::model::{AgentDefinition, AgentLimits, ContextSource, ContextSourceKind, ToolGrant};
    use crate::nodes::{NodeContext, NodeExecutor};
    use serde_json::{Value, json};

    fn triager() -> AgentDefinition {
        AgentDefinition {
            id: "triager".into(),
            instructions: Some("Be terse.".into()),
            model: Some("sonnet".into()),
            provider: Some("anthropic".into()),
            working_dir: Some("/srv/checkout".into()),
            limits: AgentLimits {
                max_steps: Some(8),
                max_tool_calls: Some(20),
                agent_timeout_secs: Some(300),
                tool_timeout_secs: Some(30),
            },
            tools: vec![
                ToolGrant::new("github.search"),
                ToolGrant {
                    slug: "github.label".into(),
                    connection_ref: Some("conn_definition".into()),
                },
            ],
            metadata: json!({ "tier": "fast" }).as_object().unwrap().clone(),
            ..Default::default()
        }
    }

    /// Runs an `agent` node against an in-graph registry and a typed harness.
    async fn run_with_registry(
        config: Value,
        agents: &[AgentDefinition],
        caps: &Capabilities,
    ) -> Value {
        let node = agent_node(config);
        let input = vec![Item::new(json!({ "seed": 1 }))];
        let run_meta = json!({ "run_id": "run_7", "sub_workflow_depth": 2 });
        let out = super::super::AgentNode
            .execute(NodeContext {
                node: &node,
                input: &input,
                run: &run_meta,
                nodes: &Value::Null,
                caps,
                agents,
                observer: &crate::observability::NoopObserver,
                token: crate::engine::CancellationToken::new(),
                lane: None,
                resume: None,
                step: 0,
            })
            .await
            .expect("execute");
        out.items[0].json.clone()
    }

    #[tokio::test]
    async fn an_in_graph_definition_reaches_the_harness_merged_with_node_overrides() {
        let caps = mock_capabilities_with_agent(MockAgentHarness::new());
        let value = run_with_registry(
            json!({
                "agent_ref": "triager",
                "prompt": "Triage it.",
                "instructions": "Prefer `bug`.",
                "model": "opus",
                "working_dir": "/srv/other",
                "tools": [{ "slug": "github.search" }],
                "limits": { "max_steps": 4 },
                "metadata": { "extra": true }
            }),
            &[triager()],
            &caps,
        )
        .await;
        let echo = &value["json"];

        assert_eq!(echo["agent"], "triager");
        assert_eq!(
            echo["instructions"], "Be terse.\n\nPrefer `bug`.",
            "node instructions append to the definition's"
        );
        assert_eq!(echo["model"], "opus", "the node overrides the model");
        assert_eq!(
            echo["provider"], "anthropic",
            "an un-overridden provider survives from the definition"
        );
        assert_eq!(echo["working_dir"], "/srv/other");
        assert_eq!(echo["prompt"], "Triage it.");
        assert_eq!(
            echo["data"][0]["seed"], 1,
            "input items ride along structurally"
        );
        assert_eq!(echo["limits"]["max_steps"], 4, "the node tightened it");
        assert_eq!(
            echo["limits"]["max_tool_calls"], 20,
            "un-narrowed bound survives"
        );
        assert_eq!(echo["limits"]["tool_timeout_secs"], 30);
        assert_eq!(echo["limits"]["agent_timeout_secs"], 300);
        assert_eq!(echo["metadata"]["tier"], "fast");
        assert_eq!(echo["metadata"]["extra"], true);
        assert_eq!(
            echo["tools"],
            json!(["github.search"]),
            "the node narrowed the definition's two grants to one"
        );
        assert_eq!(echo["identity"]["node_id"], "n");
        assert_eq!(echo["identity"]["run_id"], "run_7");
        assert_eq!(echo["identity"]["depth"], 2);
        assert_eq!(value["meta"]["stop"], "finished");
        assert_eq!(value["meta"]["agent_ref"], "triager");
        assert_eq!(value["meta"]["usage"]["steps"], 1);
    }

    #[tokio::test]
    async fn the_in_graph_registry_wins_over_the_harnesss() {
        let host_side = AgentDefinition {
            id: "triager".into(),
            model: Some("host-model".into()),
            ..Default::default()
        };
        let caps = mock_capabilities_with_agent(MockAgentHarness::new().with(host_side));
        let value = run_with_registry(json!({ "agent_ref": "triager" }), &[triager()], &caps).await;
        assert_eq!(
            value["json"]["model"], "sonnet",
            "the graph's definition wins"
        );
    }

    #[tokio::test]
    async fn the_harness_registry_answers_when_the_graph_does_not() {
        let caps = mock_capabilities_with_agent(MockAgentHarness::new().with(triager()));
        let value = run_with_registry(json!({ "agent_ref": "triager" }), &[], &caps).await;
        assert_eq!(value["json"]["model"], "sonnet");
        assert_eq!(value["json"]["provider"], "anthropic");
    }

    #[tokio::test]
    async fn an_unknown_ref_passes_through_as_a_bare_id() {
        // Not an error: the harness may resolve refs internally, which is
        // exactly what it did before a registry existed.
        let caps = mock_capabilities_with_agent(MockAgentHarness::new());
        let value = run_with_registry(json!({ "agent_ref": "mystery" }), &[], &caps).await;
        assert_eq!(value["json"]["agent"], "mystery");
        assert!(value["json"]["model"].is_null());
    }

    #[tokio::test]
    async fn context_sources_resolve_in_declaration_order() {
        let mut agent = triager();
        agent.context = vec![
            ContextSource::new(ContextSourceKind::Host {
                source: "soul".into(),
                params: json!({ "k": "v" }),
            }),
            ContextSource::new(ContextSourceKind::Memory {
                scope: "user".into(),
                query: "preferences".into(),
                limit: Some(3),
            }),
        ];
        let caps = mock_capabilities_with_agent(MockAgentHarness::new());
        let value = run_with_registry(
            json!({
                "agent_ref": "triager",
                "context": [
                    { "kind": "text", "label": "Body", "text": "=item.seed" },
                    { "kind": "items" }
                ]
            }),
            &[agent],
            &caps,
        )
        .await;
        let blocks = value["json"]["context"].as_array().expect("context blocks");

        assert_eq!(blocks.len(), 4, "definition blocks first, then the node's");
        assert_eq!(blocks[0]["kind"], "host");
        assert_eq!(blocks[0]["data"]["k"], "v");
        assert_eq!(blocks[1]["kind"], "memory");
        assert_eq!(blocks[2]["label"], "Body");
        assert_eq!(
            blocks[2]["text"], "1",
            "the =expression resolved against the item"
        );
        assert_eq!(blocks[3]["kind"], "items");
        assert_eq!(blocks[3]["data"][0]["seed"], 1);
        assert_eq!(
            blocks[3]["label"], "context_3",
            "an unlabelled block is numbered by its position in the ASSEMBLED list, \
                 not within the node's own `context` array"
        );
    }

    #[tokio::test]
    async fn an_unresolvable_context_source_fails_the_node_unless_optional() {
        let caps = mock_capabilities_with_agent(MockAgentHarness::new());
        let node = agent_node(json!({
            "agent_ref": "triager",
            "context": [{ "kind": "host", "source": "unknown" }]
        }));
        let input: Vec<Item> = vec![];
        let run_meta = Value::Null;
        let err = super::super::AgentNode
            .execute(NodeContext {
                node: &node,
                input: &input,
                run: &run_meta,
                nodes: &Value::Null,
                caps: &caps,
                agents: &[],
                observer: &crate::observability::NoopObserver,
                token: crate::engine::CancellationToken::new(),
                lane: None,
                resume: None,
                step: 0,
            })
            .await
            .expect_err("an unresolved required block must fail the node");
        let message = err.to_string();
        assert!(message.contains("could not be resolved"), "{message}");
        assert!(message.contains("optional"), "{message}");

        // ...and marking it optional makes it survivable.
        let value = run_with_registry(
            json!({
                "agent_ref": "triager",
                "context": [{ "kind": "host", "source": "unknown", "optional": true }]
            }),
            &[],
            &caps,
        )
        .await;
        assert_eq!(value["json"]["context"], json!([]));
    }

    #[tokio::test]
    async fn tool_grants_are_expanded_by_the_harness() {
        let mut agent = triager();
        agent.tools = vec![ToolGrant::new("github.*")];
        let caps = mock_capabilities_with_agent(MockAgentHarness::new());
        let value = run_with_registry(json!({ "agent_ref": "triager" }), &[agent], &caps).await;
        assert_eq!(
            value["json"]["tools"],
            json!(["github.alpha", "github.beta"]),
            "the harness expanded the namespace pattern"
        );
    }

    #[tokio::test]
    async fn a_limit_stop_is_visible_and_skips_the_output_parser() {
        let caps = mock_capabilities_with_agent(MockLimitedAgentRunner);
        let value = run_with_registry(
            json!({
                "agent_ref": "triager",
                // A schema the partial payload could never satisfy: if the
                // parser ran, this would fail the node instead of emitting.
                "output_parser": {
                    "schema": { "type": "object", "required": ["definitely_absent"] },
                    "auto_fix": false
                }
            }),
            &[],
            &caps,
        )
        .await;
        assert_eq!(value["meta"]["stop"], "limit_stop");
        assert_eq!(value["meta"]["limit"], "max_steps");
        assert_eq!(
            value["json"]["partial"], true,
            "the partial payload is kept"
        );
        assert_eq!(value["text"], "got as far as I could");
    }

    #[tokio::test]
    async fn a_paused_agent_fails_loudly_rather_than_looking_finished() {
        let caps = mock_capabilities_with_agent(MockPausingAgentRunner);
        let node = agent_node(json!({ "agent_ref": "triager" }));
        let input: Vec<Item> = vec![];
        let run_meta = Value::Null;
        let err = super::super::AgentNode
            .execute(NodeContext {
                node: &node,
                input: &input,
                run: &run_meta,
                nodes: &Value::Null,
                caps: &caps,
                agents: &[],
                observer: &crate::observability::NoopObserver,
                token: crate::engine::CancellationToken::new(),
                lane: None,
                resume: None,
                step: 0,
            })
            .await
            .expect_err("a pause must not be reported as a finished answer");
        let message = err.to_string();
        assert!(message.contains("paused"), "{message}");
        assert!(message.contains("tool_approval"), "{message}");
    }

    #[tokio::test]
    async fn declared_context_still_resolves_without_a_harness() {
        // No `AgentRunner` wired: the node degrades to a completion, but the
        // author's declared context must not be silently dropped.
        let node = agent_node(json!({
            "prompt": "hi",
            "context": [{ "kind": "memory", "scope": "user", "query": "prefs" }]
        }));
        let value = run_agent(&node, &crate::caps::mock::mock_capabilities()).await;
        let blocks = &value["json"]["completion"]["context"];
        assert_eq!(blocks[0]["source_kind"], "memory");
        assert!(
            blocks[0]["data"].get("results").is_some(),
            "the memory capability resolved the block: {blocks}"
        );
    }

    #[tokio::test]
    async fn a_legacy_host_receives_the_byte_identical_config_it_always_did() {
        // THE non-breaking guarantee, end to end: `MockAgentRunner`
        // implements only `run_agent`, so the default `run` shim applies and
        // the host sees exactly the (agent_ref, resolved config, conn) it
        // received before the typed seam existed.
        let caps = mock_capabilities_with_agent(crate::caps::mock::MockAgentRunner);
        let config = json!({
            "agent_ref": "researcher",
            "prompt": "hi",
            "connection_ref": "acct_9"
        });
        let value = run_agent(&agent_node(config.clone()), &caps).await;
        assert_eq!(value["raw"]["agent"], "researcher");
        assert_eq!(value["raw"]["request"], config);
        assert_eq!(value["raw"]["connection"], "acct_9");
        assert_eq!(value["meta"]["stop"], "finished");
    }

    #[tokio::test]
    async fn list_agents_exposes_the_harness_catalogue() {
        let harness = MockAgentHarness::new().with(triager());
        assert_eq!(harness.list_agents().await.unwrap().len(), 1);
    }
}
