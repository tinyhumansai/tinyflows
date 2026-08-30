//! Tests for the authoring gates.
//!
//! Two obligations, and the second is the one that keeps a gate trustworthy:
//! it fires on the graph that is guaranteed broken, and it stays silent on
//! everything else. A gate with false positives costs authors their edits and
//! teaches them to route around it.

use serde_json::json;

use super::*;
use crate::model::WorkflowGraph;

/// A graph from a node list, with no edges — the gates read configs, not
/// topology, and a trigger would only be noise here.
fn graph(nodes: serde_json::Value) -> WorkflowGraph {
    serde_json::from_value(json!({ "name": "test", "nodes": nodes, "edges": [] }))
        .expect("graph parses")
}

// ---- prompts written as expressions ----

#[test]
fn an_instruction_written_as_an_expression_is_refused() {
    let graph = graph(json!([
        { "id": "work", "kind": "agent", "name": "Work",
          "config": { "prompt": "=You are given an issue: .item. Summarise it" } },
    ]));

    let failures = failures(&graph);

    // `=` does not interpolate. The whole expression resolves to null and the
    // node dispatches a harness session with nothing to do.
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(failures[0].contains("does not interpolate"), "{failures:?}");
    assert!(failures[0].contains("work"), "the node has to be named");
}

#[test]
fn the_instruction_alias_is_checked_too() {
    // `instruction` is what other hosts call the same field, and the
    // engine accepts it — so a gate that only read `prompt` would miss half of
    // what authors actually write.
    let graph = graph(json!([
        { "id": "work", "kind": "agent", "name": "Work",
          "config": { "instruction": "=Look at the diff and fix it" } },
    ]));

    assert_eq!(failures(&graph).len(), 1, "{:?}", failures(&graph));
}

#[test]
fn a_plain_instruction_is_left_alone() {
    let graph = graph(json!([
        { "id": "work", "kind": "agent", "name": "Work",
          "config": { "prompt": "You are given an issue. Summarise it." } },
    ]));

    assert!(failures(&graph).is_empty());
}

#[test]
fn a_real_expression_is_not_mistaken_for_prose() {
    for expr in [
        "=.item.text",
        "=nodes.fetch.item.json.title",
        "=if .item.ok then .item.text else \"none\" end",
        "=.item.issues | map(.title) | join(\", \")",
        "=\"Summarise this issue for me\"",
    ] {
        let graph = graph(json!([
            { "id": "work", "kind": "agent", "name": "Work",
              "config": { "prompt": expr } },
        ]));

        assert!(
            failures(&graph).is_empty(),
            "{expr} is valid jq and must not be refused: {:?}",
            failures(&graph)
        );
    }
}

// ---- the output envelope ----

#[test]
fn reading_an_agents_output_without_the_envelope_is_refused() {
    let graph = graph(json!([
        { "id": "fetch-agent", "kind": "agent", "name": "Fetch", "config": { "prompt": "get it" } },
        { "id": "notify", "kind": "tool_call", "name": "Notify",
          "config": { "slug": "demo:echo", "args": { "text": "=nodes.fetch-agent.item.title" } } },
    ]));

    let failures = failures(&graph);

    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(failures[0].contains("args.text"), "{failures:?}");
    // The message has to carry the correction, not just the complaint.
    assert!(
        failures[0].contains("=nodes.fetch-agent.item.json.title"),
        "{failures:?}"
    );
}

#[test]
fn reading_through_the_envelope_is_accepted() {
    // The agent declares `title`, so the binding addresses something that will
    // exist — the envelope is only half of what makes a read resolvable.
    let graph = graph(json!([
        { "id": "fetch", "kind": "agent", "name": "Fetch", "config": {
            "prompt": "get it",
            "output_parser": { "schema": { "type": "object",
                "properties": { "title": { "type": "string" } } } }
        } },
        { "id": "notify", "kind": "tool_call", "name": "Notify",
          "config": { "slug": "demo:echo",
                      "args": { "text": "=nodes.fetch.item.json.title" } } },
    ]));

    assert!(failures(&graph).is_empty(), "{:?}", failures(&graph));
}

#[test]
fn a_node_kind_that_does_not_wrap_its_output_is_read_directly() {
    // A transform's output is the item itself, so `.item.<field>` is correct
    // there and refusing it would be a false positive.
    let graph = graph(json!([
        { "id": "shape", "kind": "transform", "name": "Shape",
          "config": { "set": { "title": "=.item.name" } } },
        { "id": "notify", "kind": "tool_call", "name": "Notify",
          "config": { "slug": "demo:echo", "args": { "text": "=nodes.shape.item.title" } } },
    ]));

    assert!(failures(&graph).is_empty(), "{:?}", failures(&graph));
}

#[test]
fn a_binding_nested_deep_inside_args_is_still_found_and_named() {
    let graph = graph(json!([
        { "id": "fetch", "kind": "agent", "name": "Fetch", "config": { "prompt": "get it" } },
        { "id": "notify", "kind": "tool_call", "name": "Notify",
          "config": { "slug": "demo:echo", "args": {
              "blocks": [{ "fields": { "value": "=nodes.fetch.item.title" } }] } } },
    ]));

    let failures = failures(&graph);

    assert_eq!(failures.len(), 1, "{failures:?}");
    // Named precisely enough for an author to find it in a large config.
    assert!(
        failures[0].contains("args.blocks.0.fields.value"),
        "{failures:?}"
    );
}

#[test]
fn a_binding_to_a_node_that_does_not_exist_is_left_to_the_engine() {
    let graph = graph(json!([
        { "id": "notify", "kind": "tool_call", "name": "Notify",
          "config": { "slug": "demo:echo", "args": { "text": "=nodes.ghost.item.title" } } },
    ]));

    // The engine already reports a reference to a node that is not there;
    // saying it twice in different words helps nobody.
    assert!(failures(&graph).is_empty(), "{:?}", failures(&graph));
}

#[test]
fn an_expression_that_is_not_a_node_binding_is_not_second_guessed() {
    let graph = graph(json!([
        { "id": "fetch", "kind": "agent", "name": "Fetch", "config": {} },
        { "id": "notify", "kind": "tool_call", "name": "Notify",
          "config": { "slug": "demo:echo",
                      "args": { "text": "=.item.text | ascii_downcase",
                                "count": "=run.trigger.n",
                                "fallback": "=nodes.fetch.item.missing // \"fallback\"" } } },
    ]));

    // A gate that guessed at arbitrary jq would refuse graphs that work.
    assert!(failures(&graph).is_empty(), "{:?}", failures(&graph));
}

include!("indexed_binding_tests.rs");

// ---- the error surface ----

#[test]
fn every_failure_is_reported_at_once_rather_than_the_first() {
    let graph = graph(json!([
        { "id": "fetch", "kind": "agent", "name": "Fetch",
          "config": { "prompt": "=Go and fetch the issues" } },
        { "id": "notify", "kind": "tool_call", "name": "Notify",
          "config": { "slug": "demo:echo", "args": { "text": "=nodes.fetch.item.title" } } },
    ]));

    let messages = failures(&graph);

    // One round trip has to tell an agent everything, or it spends a turn per
    // mistake.
    assert_eq!(messages.len(), 2, "{messages:?}");
}

#[test]
fn a_clean_graph_passes() {
    let graph = graph(json!([
        { "id": "t", "kind": "trigger", "name": "Start",
          "config": { "trigger_kind": "manual" } },
        { "id": "work", "kind": "agent", "name": "Work",
          "config": { "prompt": "summarise the open issues" } },
    ]));

    assert!(failures(&graph).is_empty(), "{:?}", failures(&graph));
}

// ---- code node languages ----

#[test]
fn a_code_node_asking_for_shell_is_refused_and_pointed_at_the_shell_node() {
    let graph = graph(json!([
        { "id": "compute", "kind": "code", "name": "Compute",
          "config": { "language": "shell", "source": "echo hi" } },
    ]));

    let failures = failures(&graph);

    // The engine treats anything but the literal "python" as JavaScript, so
    // this would run a shell script through node and fail with a syntax error
    // naming an interpreter the author never chose.
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(failures[0].contains("use a `shell` node"), "{failures:?}");
}

#[test]
fn a_near_miss_language_spelling_is_refused_rather_than_silently_becoming_javascript() {
    for spelling in ["python3", "py", "js", "node"] {
        let graph = graph(json!([
            { "id": "compute", "kind": "code", "name": "Compute",
              "config": { "language": spelling, "source": "print(1)" } },
        ]));

        assert_eq!(
            failures(&graph).len(),
            1,
            "{spelling} must not silently become javascript"
        );
    }
}

#[test]
fn the_two_spellings_the_engine_actually_reads_are_accepted() {
    for spelling in ["javascript", "python"] {
        let graph = graph(json!([
            { "id": "compute", "kind": "code", "name": "Compute",
              "config": { "language": spelling, "source": "x" } },
        ]));

        assert!(
            failures(&graph).is_empty(),
            "{spelling} is exactly what the engine matches: {:?}",
            failures(&graph)
        );
    }
}

#[test]
fn a_code_node_that_names_no_language_is_left_alone() {
    let graph = graph(json!([
        { "id": "compute", "kind": "code", "name": "Compute",
          "config": { "source": "console.log(1)" } },
    ]));

    // Absent is legal and means JavaScript, which the engine's own default
    // already says — refusing it would be a false positive.
    assert!(failures(&graph).is_empty(), "{:?}", failures(&graph));
}

// ---- a prompt that is never read ----

/// A node with real `messages` never runs on `prompt`: both completion paths
/// fall through to the messages array once the prompt resolves to null. So the
/// prose prompt beside them is vestigial, and refusing the graph for it would
/// be a refusal with no failure behind it — the exact false positive that makes
/// authors route around a gate.
#[test]
fn a_prose_prompt_beside_real_messages_is_not_refused() {
    let graph = graph(json!([
        { "id": "work", "kind": "agent", "name": "Work", "config": {
            "prompt": "=You are given an issue: .item. Summarise it",
            "messages": [{ "role": "user", "content": "Summarise the issue." }]
        } },
    ]));

    assert!(failures(&graph).is_empty(), "{:?}", failures(&graph));
}

/// The escape hatch is the presence of messages that actually carry the turn.
/// An empty array carries nothing, so the prompt is still what runs.
#[test]
fn an_empty_messages_array_does_not_excuse_a_prose_prompt() {
    let graph = graph(json!([
        { "id": "work", "kind": "agent", "name": "Work", "config": {
            "prompt": "=You are given an issue: .item. Summarise it",
            "messages": []
        } },
    ]));

    assert_eq!(failures(&graph).len(), 1, "{:?}", failures(&graph));
}

// ---- tool args bound to fields an agent never declares ----

#[test]
fn a_tool_arg_reading_a_field_the_agent_schema_omits_is_refused() {
    let graph = graph(json!([
        { "id": "draft", "kind": "agent", "name": "Draft", "config": {
            "output_parser": { "schema": { "type": "object",
                "properties": { "body": { "type": "string" } } } }
        } },
        { "id": "send", "kind": "tool_call", "name": "Send", "config": {
            "slug": "GMAIL_SEND_EMAIL",
            "args": { "subject": "=nodes.draft.item.json.subject" }
        } },
    ]));

    let failures = failures(&graph);
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(failures[0].contains("subject"), "{failures:?}");
    assert!(failures[0].contains("output_parser.schema"), "{failures:?}");
}

#[test]
fn a_tool_arg_reading_a_declared_field_is_accepted() {
    let graph = graph(json!([
        { "id": "draft", "kind": "agent", "name": "Draft", "config": {
            "output_parser": { "schema": { "type": "object",
                "properties": { "subject": { "type": "string" } } } }
        } },
        { "id": "send", "kind": "tool_call", "name": "Send", "config": {
            "slug": "GMAIL_SEND_EMAIL",
            "args": { "subject": "=nodes.draft.item.json.subject" }
        } },
    ]));

    assert!(failures(&graph).is_empty(), "{:?}", failures(&graph));
}

/// A schema declares its top-level properties; how deep a value nests below one
/// is the model's business, so only the first segment is compared.
#[test]
fn only_the_first_path_segment_is_checked_against_the_schema() {
    let graph = graph(json!([
        { "id": "draft", "kind": "agent", "name": "Draft", "config": {
            "output_parser": { "schema": { "type": "object",
                "properties": { "data": { "type": "object" } } } }
        } },
        { "id": "send", "kind": "tool_call", "name": "Send", "config": {
            "slug": "GMAIL_SEND_EMAIL",
            "args": { "to": "=nodes.draft.item.json.data.recipients.0" }
        } },
    ]));

    assert!(failures(&graph).is_empty(), "{:?}", failures(&graph));
}

/// The gate is scoped to a tool call's `args`. An agent's own prompt has no
/// schema to check a mention against, and a vaguer answer is not a broken call.
#[test]
fn an_agent_prompt_mentioning_an_undeclared_field_is_left_alone() {
    let graph = graph(json!([
        { "id": "draft", "kind": "agent", "name": "Draft", "config": {
            "output_parser": { "schema": { "type": "object",
                "properties": { "body": { "type": "string" } } } }
        } },
        { "id": "review", "kind": "agent", "name": "Review", "config": {
            "prompt": "Check the subject at =nodes.draft.item.json.subject"
        } },
    ]));

    assert!(failures(&graph).is_empty(), "{:?}", failures(&graph));
}

/// A schema-less agent may return structured JSON through its host runner, so
/// the field is unverifiable rather than guaranteed absent.
#[test]
fn a_binding_to_an_agent_that_declares_no_schema_is_accepted() {
    let graph = graph(json!([
        { "id": "draft", "kind": "agent", "name": "Draft", "config": {} },
        { "id": "send", "kind": "tool_call", "name": "Send", "config": {
            "slug": "GMAIL_SEND_EMAIL",
            "args": { "subject": "=nodes.draft.item.json.subject" }
        } },
    ]));

    assert!(failures(&graph).is_empty(), "{:?}", failures(&graph));
}

/// A binding that skipped the envelope is already reported by the envelope
/// gate. Reporting it again in different words reads as two separate problems
/// and sends the author looking for a second thing to fix.
#[test]
fn a_binding_missing_the_envelope_is_reported_once_not_twice() {
    let graph = graph(json!([
        { "id": "draft", "kind": "agent", "name": "Draft", "config": {
            "output_parser": { "schema": { "type": "object",
                "properties": { "body": { "type": "string" } } } }
        } },
        { "id": "send", "kind": "tool_call", "name": "Send", "config": {
            "slug": "GMAIL_SEND_EMAIL",
            "args": { "subject": "=nodes.draft.item.subject" }
        } },
    ]));

    let failures = failures(&graph);
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(failures[0].contains("{json, text, raw}"), "{failures:?}");
}

#[test]
fn a_tool_arg_bound_to_a_transform_node_is_not_schema_checked() {
    let graph = graph(json!([
        { "id": "shape", "kind": "transform", "name": "Shape", "config": {} },
        { "id": "send", "kind": "tool_call", "name": "Send", "config": {
            "slug": "GMAIL_SEND_EMAIL",
            "args": { "subject": "=nodes.shape.item.subject" }
        } },
    ]));

    assert!(failures(&graph).is_empty(), "{:?}", failures(&graph));
}

// ---- the envelope's own accessors are not envelope violations ----

/// `.item.text` is how you read an agent's completion text, and `.item.raw` the
/// untouched response. Flagging either as "you forgot `.json`" refuses a
/// correct graph — and the suggested fix, `.item.json.text`, is a path that
/// does not exist.
#[test]
fn reading_the_envelopes_own_fields_off_an_agent_is_accepted() {
    for field in ["text", "raw", "json"] {
        let graph = graph(json!([
            { "id": "draft", "kind": "agent", "name": "Draft", "config": {} },
            { "id": "shape", "kind": "transform", "name": "Shape",
              "config": { "set": { "out": format!("=nodes.draft.item.{field}") } } },
        ]));

        assert!(
            failures(&graph).is_empty(),
            "`.item.{field}` addresses the envelope itself: {:?}",
            failures(&graph)
        );
    }
}

/// Only the first segment is the envelope. `text_body` is a real field someone
/// is reading through the envelope and has forgotten the `.json` on, so it must
/// still be refused — the exemption is for the accessor, not for any name that
/// happens to start with one.
#[test]
fn a_field_merely_prefixed_like_an_envelope_accessor_is_still_refused() {
    let graph = graph(json!([
        { "id": "draft", "kind": "agent", "name": "Draft", "config": {} },
        { "id": "shape", "kind": "transform", "name": "Shape",
          "config": { "set": { "out": "=nodes.draft.item.text_body" } } },
    ]));

    let failures = failures(&graph);
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(failures[0].contains("{json, text, raw}"), "{failures:?}");
}

/// Reading a nested path *through* the envelope accessor stays fine.
#[test]
fn a_nested_path_under_an_envelope_accessor_is_accepted() {
    let graph = graph(json!([
        { "id": "draft", "kind": "agent", "name": "Draft", "config": {
            "output_parser": { "schema": { "type": "object",
                "properties": { "body": { "type": "string" } } } }
        } },
        { "id": "shape", "kind": "transform", "name": "Shape",
          "config": { "set": { "out": "=nodes.draft.item.json.body" } } },
    ]));

    assert!(failures(&graph).is_empty(), "{:?}", failures(&graph));
}
