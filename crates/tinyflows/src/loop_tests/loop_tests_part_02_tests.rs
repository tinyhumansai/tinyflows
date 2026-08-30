
/// A cycle with no `loop` node is legal as long as the trigger declares a
/// `recursion_limit` — the bound just has to come from somewhere.
#[test]
fn a_recursion_limit_bounds_a_loopless_cycle() {
    let graph = WorkflowGraph {
        nodes: vec![
            node_cfg(
                "t",
                NodeKind::Trigger,
                serde_json::json!({ "recursion_limit": 10 }),
            ),
            node("a", NodeKind::OutputParser),
            node("b", NodeKind::OutputParser),
        ],
        edges: vec![
            edge_on("t", "main", "a"),
            edge_on("a", "main", "b"),
            edge_on("b", "main", "a"),
        ],
        ..Default::default()
    };
    assert_eq!(validate_all(&graph), Vec::new());
}

// ---- agent registry + agent-node configuration ------------------------

mod agents {
    use super::{node, node_cfg};
    use crate::error::ValidationError;
    use crate::model::{AgentDefinition, NodeKind, WorkflowGraph};
    use crate::validate::{unresolved_agent_refs, validate_all};
    use serde_json::json;

    /// A one-agent-node graph with the given registry and node config.
    fn agent_graph(agents: Vec<AgentDefinition>, config: serde_json::Value) -> WorkflowGraph {
        WorkflowGraph {
            agents,
            nodes: vec![
                node("t", NodeKind::Trigger),
                node_cfg("a", NodeKind::Agent, config),
            ],
            edges: vec![crate::model::Edge {
                from_node: "t".into(),
                from_port: "main".into(),
                to_node: "a".into(),
                to_port: "main".into(),
            }],
            ..Default::default()
        }
    }

    fn reasons(graph: &WorkflowGraph) -> Vec<String> {
        validate_all(graph)
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    #[test]
    fn a_well_formed_registry_and_node_validate_clean() {
        let agent: AgentDefinition = serde_json::from_value(json!({
            "id": "triager",
            "model": "opus",
            "provider": "anthropic",
            "tools": [{ "slug": "github.*" }],
            "context": [{ "kind": "memory", "scope": "user", "query": "prefs" }],
            "limits": { "max_steps": 8, "tool_timeout_secs": 30 }
        }))
        .unwrap();
        let graph = agent_graph(
            vec![agent],
            json!({
                "agent_ref": "triager",
                "prompt": "go",
                "tools": [{ "slug": "github.search" }],
                "limits": { "max_steps": 4 }
            }),
        );
        assert_eq!(validate_all(&graph), Vec::new());
    }

    #[test]
    fn duplicate_agent_ids_are_rejected() {
        let graph = agent_graph(
            vec![AgentDefinition::new("dup"), AgentDefinition::new("dup")],
            json!({}),
        );
        assert!(
            validate_all(&graph).contains(&ValidationError::DuplicateAgentId("dup".into())),
            "{:?}",
            validate_all(&graph)
        );
    }

    #[test]
    fn an_empty_agent_id_is_rejected() {
        let graph = agent_graph(vec![AgentDefinition::new("")], json!({}));
        assert!(reasons(&graph).iter().any(|r| r.contains("non-empty `id`")));
    }

    #[test]
    fn an_expression_agent_ref_is_rejected() {
        // The escalation guard: an expression resolves from run data, which
        // could include model output, and would choose the agent's tools.
        let graph = agent_graph(vec![], json!({ "agent_ref": "=item.which_agent" }));
        assert!(
            reasons(&graph)
                .iter()
                .any(|r| r.contains("must be a literal")),
            "{:?}",
            reasons(&graph)
        );
    }

    #[test]
    fn an_expression_tool_slug_or_connection_is_rejected() {
        let by_slug = agent_graph(vec![], json!({ "tools": [{ "slug": "=item.tool" }] }));
        assert!(
            reasons(&by_slug)
                .iter()
                .any(|r| r.contains("`slug` must be a literal"))
        );

        let by_conn = agent_graph(
            vec![],
            json!({ "tools": [{ "slug": "ok", "connection_ref": "=item.acct" }] }),
        );
        assert!(
            reasons(&by_conn)
                .iter()
                .any(|r| r.contains("`connection_ref` must be a literal"))
        );
    }

    #[test]
    fn only_a_trailing_dot_star_is_a_valid_tool_pattern() {
        for bad in ["*", "*.post", "sla*ck"] {
            let g = agent_graph(vec![], json!({ "tools": [{ "slug": bad }] }));
            assert!(
                reasons(&g).iter().any(|r| r.contains("valid pattern")),
                "{bad:?} should be rejected: {:?}",
                reasons(&g)
            );
        }
        let good = agent_graph(vec![], json!({ "tools": [{ "slug": "slack.*" }] }));
        assert_eq!(validate_all(&good), Vec::new());
    }

    #[test]
    fn a_node_may_not_widen_its_agents_tool_grants() {
        let mut agent = AgentDefinition::new("triager");
        agent.tools = vec![crate::model::ToolGrant::new("github.search")];
        let graph = agent_graph(
            vec![agent],
            json!({ "agent_ref": "triager", "tools": [{ "slug": "shell.exec" }] }),
        );
        assert!(
            reasons(&graph)
                .iter()
                .any(|r| r.contains("is not granted by agent")),
            "{:?}",
            reasons(&graph)
        );
    }

    #[test]
    fn an_unknown_memory_scope_is_rejected() {
        let graph = agent_graph(
            vec![],
            json!({ "context": [{ "kind": "memory", "scope": "=item.scope", "query": "q" }] }),
        );
        assert!(
            reasons(&graph)
                .iter()
                .any(|r| r.contains("unknown memory scope")),
            "{:?}",
            reasons(&graph)
        );
    }

    #[test]
    fn a_zero_limit_is_rejected() {
        let graph = agent_graph(vec![], json!({ "limits": { "max_steps": 0 } }));
        assert!(
            reasons(&graph).iter().any(|r| r.contains("greater than 0")),
            "{:?}",
            reasons(&graph)
        );
    }

    #[test]
    fn a_malformed_context_or_limits_block_reports_the_key() {
        let graph = agent_graph(vec![], json!({ "context": "not an array" }));
        assert!(
            reasons(&graph)
                .iter()
                .any(|r| r.contains("invalid `context`"))
        );
    }

    #[test]
    fn an_unresolved_agent_ref_is_deferred_not_an_error() {
        // Validation runs without capabilities and the harness registry is
        // the documented fallback, so this is valid — but reportable.
        let graph = agent_graph(vec![], json!({ "agent_ref": "host_side" }));
        assert_eq!(validate_all(&graph), Vec::new());
        assert_eq!(
            unresolved_agent_refs(&graph),
            vec![("a".to_string(), "host_side".to_string())]
        );

        let declared = agent_graph(
            vec![AgentDefinition::new("host_side")],
            json!({ "agent_ref": "host_side" }),
        );
        assert!(unresolved_agent_refs(&declared).is_empty());
    }

    #[test]
    fn the_new_error_variants_carry_stable_codes() {
        assert_eq!(
            ValidationError::DuplicateAgentId("x".into()).code(),
            "duplicate_agent_id"
        );
        assert_eq!(
            ValidationError::InvalidAgentDefinition {
                agent: "x".into(),
                reason: "y".into()
            }
            .code(),
            "invalid_agent_definition"
        );
    }
}
