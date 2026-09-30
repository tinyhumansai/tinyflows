use super::*;

// ---- prompts that are real jq, and literal args ----
//
// Ported from OpenHuman's host-side gate tests, which pinned these scenarios
// through a wrapper around this module.

#[test]
fn a_jq_string_concatenation_prompt_is_accepted() {
    let graph = graph(json!([
        { "id": "greet", "kind": "agent", "name": "Greet",
          "config": { "prompt": "=\"Hi \" + .item.name" } },
    ]));

    assert!(failures(&graph).is_empty(), "{:?}", failures(&graph));
}

/// Regression for the quote-toggle desync: an escaped quote inside a jq string
/// literal must not flip the string-stripping pass's in-string state, or the
/// text between the escaped and the closing quote leaks out as bare code and
/// trips the prose heuristic.
#[test]
fn an_escaped_quote_inside_a_jq_string_is_not_mistaken_for_prose() {
    let graph = graph(json!([
        { "id": "greet", "kind": "agent", "name": "Greet",
          "config": { "prompt": "=\"Say \\\"hello world\\\" nicely\" + .item.name" } },
    ]));

    assert!(failures(&graph).is_empty(), "{:?}", failures(&graph));
}

#[test]
fn literal_tool_args_are_not_inspected() {
    let graph = graph(json!([
        { "id": "post", "kind": "tool_call", "name": "Post",
          "config": { "slug": "SLACK_SEND_MESSAGE",
            "args": { "channel": "general", "count": 3, "cc": ["a@b.com"] } } },
    ]));

    assert!(failures(&graph).is_empty(), "{:?}", failures(&graph));
}

#[test]
fn a_binding_to_a_schema_less_agent_is_unverifiable_not_refused() {
    let graph = graph(json!([
        { "id": "summarize", "kind": "agent", "name": "Summarize",
          "config": { "agent_ref": "researcher", "prompt": "summarize" } },
        { "id": "post", "kind": "tool_call", "name": "Post",
          "config": { "slug": "SLACK_SEND_MESSAGE",
            "args": { "channel": "=nodes.summarize.item.json.channel" } } },
    ]));

    assert!(failures(&graph).is_empty(), "{:?}", failures(&graph));
}

/// Skipping `.json` is refused even when the agent declares a matching schema:
/// the fault is the envelope, not the field inside it.
#[test]
fn skipping_the_envelope_is_refused_even_when_the_schema_matches() {
    let graph = graph(json!([
        { "id": "summarize", "kind": "agent", "name": "Summarize",
          "config": { "prompt": "summarize",
            "output_parser": { "schema": { "type": "object",
                "properties": { "channel": { "type": "string" } } } } } },
        { "id": "post", "kind": "tool_call", "name": "Post",
          "config": { "slug": "SLACK_SEND_MESSAGE",
            "args": { "channel": "=nodes.summarize.item.channel" } } },
    ]));

    let failures = failures(&graph);
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(
        failures[0].contains("Fix: `=nodes.summarize.item.json.channel`."),
        "{failures:?}"
    );
}
