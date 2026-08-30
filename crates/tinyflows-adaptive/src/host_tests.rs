use super::*;

#[test]
fn a_host_that_configured_any_rendered_fact_is_not_unknown() {
    // `is_unknown` must test every field `render` prints: a fact it skips
    // is one that silently never reaches the authoring prompt.
    for facts in [
        HostFacts {
            default_harness: Some("codex".into()),
            ..HostFacts::unknown()
        },
        HostFacts {
            default_model: Some("gpt-5".into()),
            ..HostFacts::unknown()
        },
        HostFacts {
            max_parallel_agents: Some(2),
            ..HostFacts::unknown()
        },
        HostFacts {
            run_timeout_secs: Some(600),
            ..HostFacts::unknown()
        },
        HostFacts {
            tools: vec![ToolFact {
                slug: "host:shell".into(),
                args: "`script` (inline) or `script_path`".into(),
            }],
            ..HostFacts::unknown()
        },
    ] {
        assert!(!facts.is_unknown(), "{facts:?}");
        assert!(!facts.render().is_empty(), "and it renders");
    }
}

#[test]
fn host_names_compare_case_insensitively() {
    // DNS is case-insensitive; `API.GitHub.com` against `github.com` must
    // not cost the episode a spurious authoring round.
    let facts = HostFacts {
        http_allowlist: vec!["github.com".into()],
        ..HostFacts::unknown()
    };
    let graph = graph(vec![node(
        "fetch",
        NodeKind::HttpRequest,
        serde_json::json!({ "url": "https://API.GitHub.com/repos/x", "method": "GET" }),
    )]);
    assert!(facts.check(&graph).is_empty(), "{:?}", facts.check(&graph));
}
use serde_json::json;
use tinyflows::model::Node;

fn node(id: &str, kind: NodeKind, config: Value) -> Node {
    Node {
        id: id.into(),
        kind,
        type_version: 1,
        name: id.into(),
        config,
        ports: Vec::new(),
        position: None,
    }
}

fn graph(nodes: Vec<Node>) -> WorkflowGraph {
    WorkflowGraph {
        nodes,
        ..WorkflowGraph::default()
    }
}

#[test]
fn a_host_that_has_said_nothing_refuses_nothing() {
    // The reading that would break every unconfigured deployment: empty
    // meaning "deny" rather than "unknown".
    let facts = HostFacts::unknown();
    let g = graph(vec![
        node("a", NodeKind::Agent, json!({ "agent_ref": "anyone" })),
        node(
            "t",
            NodeKind::ToolCall,
            json!({ "slug": "anything:at:all" }),
        ),
        node(
            "c",
            NodeKind::Code,
            json!({ "language": "python", "source": "1" }),
        ),
    ]);
    assert!(facts.check(&g).is_empty());
    assert!(
        facts.render().is_empty(),
        "nothing known renders as nothing"
    );
}

#[test]
fn a_worker_this_host_does_not_have_is_named() {
    let facts = HostFacts {
        workers: vec!["laptop".into(), "ci".into()],
        ..HostFacts::unknown()
    };
    let problems = facts.check(&graph(vec![node(
        "a",
        NodeKind::Agent,
        json!({ "agent_ref": "desktop" }),
    )]));
    assert_eq!(problems.len(), 1);
    assert!(problems[0].contains("desktop"), "{problems:?}");
    assert!(
        problems[0].contains("laptop, ci"),
        "the alternatives are offered"
    );
}

#[test]
fn no_default_worker_makes_agent_ref_mandatory() {
    // A host fact that changes a field from optional to required.
    let facts = HostFacts {
        workers: vec!["laptop".into()],
        default_worker: None,
        ..HostFacts::unknown()
    };
    let problems = facts.check(&graph(vec![node("a", NodeKind::Agent, json!({}))]));
    assert_eq!(problems.len(), 1);
    assert!(problems[0].contains("must name"), "{problems:?}");
}

#[test]
fn a_default_worker_makes_a_bare_agent_node_fine() {
    let facts = HostFacts {
        workers: vec!["laptop".into()],
        default_worker: Some("laptop".into()),
        ..HostFacts::unknown()
    };
    assert!(
        facts
            .check(&graph(vec![node("a", NodeKind::Agent, json!({}))]))
            .is_empty()
    );
}

#[test]
fn a_slug_outside_both_lists_is_refused() {
    let facts = HostFacts {
        native_tools: vec!["medulla:shell".into()],
        tool_allowlist: vec!["github".into()],
        ..HostFacts::unknown()
    };
    let g = graph(vec![
        node("ok", NodeKind::ToolCall, json!({ "slug": "medulla:shell" })),
        node("no", NodeKind::ToolCall, json!({ "slug": "slack" })),
    ]);
    let problems = facts.check(&g);
    assert_eq!(problems.len(), 1);
    assert!(problems[0].contains("slack"), "{problems:?}");
}

#[test]
fn an_http_host_outside_the_allowlist_is_refused_but_a_subdomain_is_not() {
    let facts = HostFacts {
        http_allowlist: vec!["github.com".into()],
        ..HostFacts::unknown()
    };
    let g = graph(vec![
        node(
            "ok",
            NodeKind::HttpRequest,
            json!({ "url": "https://api.github.com/x" }),
        ),
        node(
            "no",
            NodeKind::HttpRequest,
            json!({ "url": "https://evil.test/x" }),
        ),
    ]);
    let problems = facts.check(&g);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].contains("evil.test"));
}

#[test]
fn a_url_built_from_an_expression_is_left_to_run_time() {
    // Refusing it would refuse the correct way to write a parameterised
    // request, which is the thing the authoring prompt asks for.
    let facts = HostFacts {
        http_allowlist: vec!["github.com".into()],
        ..HostFacts::unknown()
    };
    let g = graph(vec![node(
        "u",
        NodeKind::HttpRequest,
        json!({ "url": "=\"https://\" + .inputs.host" }),
    )]);
    assert!(facts.check(&g).is_empty());
}

#[test]
fn disabled_code_and_refused_shell_are_both_reported() {
    let facts = HostFacts {
        allow_code: Some(false),
        shell_available: Some(false),
        ..HostFacts::unknown()
    };
    let g = graph(vec![
        node(
            "c",
            NodeKind::Code,
            json!({ "language": "python", "source": "1" }),
        ),
        node("s", NodeKind::Shell, json!({ "script": "ls" })),
    ]);
    assert_eq!(
        facts.check(&g).len(),
        2,
        "every failure at once, not the first"
    );
}

#[test]
fn a_loop_above_the_host_ceiling_is_reported() {
    // Otherwise it silently stops earlier than the graph says.
    let facts = HostFacts {
        max_loop_iterations: Some(10),
        ..HostFacts::unknown()
    };
    let g = graph(vec![node(
        "l",
        NodeKind::Loop,
        json!({ "max_iterations": 50 }),
    )]);
    let problems = facts.check(&g);
    assert_eq!(problems.len(), 1);
    assert!(problems[0].contains("ceiling of 10"), "{problems:?}");
}

#[test]
fn a_trigger_kind_that_never_fires_is_reported() {
    let facts = HostFacts {
        trigger_kinds: vec!["manual".into()],
        ..HostFacts::unknown()
    };
    let g = graph(vec![node(
        "t",
        NodeKind::Trigger,
        json!({ "trigger_kind": "schedule" }),
    )]);
    let problems = facts.check(&g);
    assert_eq!(problems.len(), 1);
    assert!(problems[0].contains("never dispatched"), "{problems:?}");
}

#[test]
fn a_tool_fact_renders_its_argument_shape_into_the_prompt() {
    let facts = HostFacts {
        native_tools: vec!["host:shell".into()],
        tools: vec![ToolFact {
            slug: "host:shell".into(),
            args: "`script` (inline text) or `script_path` (a file); NOT `command`".into(),
        }],
        ..HostFacts::unknown()
    };
    let rendered = facts.render();
    assert!(
        rendered.contains("tool `host:shell` args:") && rendered.contains("script_path"),
        "{rendered}"
    );
}

#[test]
fn the_rendering_states_consequences_not_just_values() {
    let facts = HostFacts {
        default_worker: None,
        workers: vec!["laptop".into()],
        allow_code: Some(false),
        notes: vec!["Only manual triggers fire here.".into()],
        ..HostFacts::unknown()
    };
    let rendered = facts.render();
    assert!(rendered.contains("every agent node must name config.agent_ref"));
    assert!(rendered.contains("DISABLED"));
    assert!(rendered.contains("Only manual triggers fire here."));
}

#[test]
fn a_url_without_a_scheme_still_yields_its_host() {
    assert_eq!(host_of("api.github.com/x"), Some("api.github.com"));
    assert_eq!(
        host_of("https://user:pw@api.github.com:443/x"),
        Some("api.github.com")
    );
    assert_eq!(host_of(""), None);
}
