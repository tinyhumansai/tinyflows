//! Tests for the sandbox preflight.
//!
//! The obligation that matters here is the second one: a null the sandbox
//! cannot distinguish from a real value must not become a refusal. Three
//! classes of null are legitimate — trigger-scoped data, opaque upstream tool
//! output, and a run that never settled — and each has burned a correct graph
//! before, so each gets a test that would fail if it were reported.

use serde_json::json;

use super::*;

/// A graph from a node list and an edge list. `preflight` runs the engine, so
/// unlike the static gates it needs real topology and a trigger.
fn graph(nodes: serde_json::Value, edges: serde_json::Value) -> WorkflowGraph {
    serde_json::from_value(json!({ "name": "test", "nodes": nodes, "edges": edges }))
        .expect("graph parses")
}

fn trigger() -> serde_json::Value {
    json!({ "id": "t", "kind": "trigger", "name": "Manual",
            "config": { "trigger_kind": "manual" } })
}

/// The failure this exists for: an argument wired to a field an upstream node
/// never produces. `shape` is a `transform`, whose real output the sandbox
/// *does* produce, so a null here is not the mock's fault — it is the graph's.
#[tokio::test]
async fn an_arg_reading_a_field_an_upstream_node_never_produces_is_refused() {
    let g = graph(
        json!([
            trigger(),
            { "id": "shape", "kind": "transform", "name": "Shape",
              "config": { "expression": "={ body: \"hi\" }" } },
            { "id": "send", "kind": "tool_call", "name": "Send", "config": {
                "slug": "GMAIL_SEND_EMAIL",
                "args": { "subject": "=nodes.shape.item.subject" }
            } },
        ]),
        json!([{ "from_node": "t", "to_node": "shape" },
               { "from_node": "shape", "to_node": "send" }]),
    );

    let errors = unresolvable_tool_args(&g, &["oh:"]).await;
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].contains("subject"), "{errors:?}");
}

/// The sandbox runs on an empty trigger payload, so anything read from the
/// trigger is null *here* and populated in a real run. Refusing these would
/// refuse nearly every workflow ever written.
#[tokio::test]
async fn a_null_read_from_the_trigger_is_not_refused() {
    let g = graph(
        json!([
            trigger(),
            { "id": "send", "kind": "tool_call", "name": "Send", "config": {
                "slug": "GMAIL_SEND_EMAIL",
                "args": { "subject": "=item.subject", "to": "=run.caller" }
            } },
        ]),
        json!([{ "from_node": "t", "to_node": "send" }]),
    );

    assert!(
        unresolvable_tool_args(&g, &["oh:"]).await.is_empty(),
        "{:?}",
        unresolvable_tool_args(&g, &["oh:"]).await
    );
}

/// A mock renders a `tool_call` as an echo and can never produce the provider's
/// real output fields, so a binding onto one is unverifiable rather than
/// broken. This case is why the gate cannot simply report every null.
#[tokio::test]
async fn a_null_read_from_an_upstream_tool_call_is_not_refused() {
    let g = graph(
        json!([
            trigger(),
            { "id": "fetch", "kind": "tool_call", "name": "Fetch",
              "config": { "slug": "GMAIL_FETCH_EMAILS", "args": {} } },
            { "id": "send", "kind": "tool_call", "name": "Send", "config": {
                "slug": "GMAIL_SEND_EMAIL",
                "args": { "subject": "=nodes.fetch.item.json.data.subject" }
            } },
        ]),
        json!([{ "from_node": "t", "to_node": "fetch" },
               { "from_node": "fetch", "to_node": "send" }]),
    );

    assert!(
        unresolvable_tool_args(&g, &["oh:"]).await.is_empty(),
        "{:?}",
        unresolvable_tool_args(&g, &["oh:"]).await
    );
}

/// A host's own tool has no external provider to reject the call, which is the
/// only failure this gate protects against.
#[tokio::test]
async fn a_hosts_native_tool_call_is_skipped_entirely() {
    let g = graph(
        json!([
            trigger(),
            { "id": "shape", "kind": "transform", "name": "Shape",
              "config": { "expression": "={ body: \"hi\" }" } },
            { "id": "send", "kind": "tool_call", "name": "Send", "config": {
                "slug": "oh:send_message",
                "args": { "subject": "=nodes.shape.item.subject" }
            } },
        ]),
        json!([{ "from_node": "t", "to_node": "shape" },
               { "from_node": "shape", "to_node": "send" }]),
    );

    assert!(
        unresolvable_tool_args(&g, &["oh:"]).await.is_empty(),
        "{:?}",
        unresolvable_tool_args(&g, &["oh:"]).await
    );
    // …and it is the prefix that does it, not the node kind: with no prefixes
    // declared, the same graph is refused.
    assert_eq!(unresolvable_tool_args(&g, &[]).await.len(), 1);
}

/// A slug resolved from runtime data is not something a static gate can reason
/// about at all.
#[tokio::test]
async fn a_dynamic_slug_is_skipped() {
    let g = graph(
        json!([
            trigger(),
            { "id": "shape", "kind": "transform", "name": "Shape",
              "config": { "expression": "={ body: \"hi\" }" } },
            { "id": "send", "kind": "tool_call", "name": "Send", "config": {
                "slug": "=item.slug",
                "args": { "subject": "=nodes.shape.item.subject" }
            } },
        ]),
        json!([{ "from_node": "t", "to_node": "shape" },
               { "from_node": "shape", "to_node": "send" }]),
    );

    assert!(
        unresolvable_tool_args(&g, &["oh:"]).await.is_empty(),
        "{:?}",
        unresolvable_tool_args(&g, &["oh:"]).await
    );
}

/// An agent that declares a schema really does produce those fields under the
/// schema-aware mocks — which is the whole reason those mocks exist. Binding to
/// one must not be reported.
#[tokio::test]
async fn a_binding_to_a_declared_agent_field_resolves_under_the_schema_aware_mocks() {
    let g = graph(
        json!([
            trigger(),
            { "id": "draft", "kind": "agent", "name": "Draft", "config": {
                "prompt": "Draft a subject line.",
                "output_parser": { "schema": { "type": "object",
                    "properties": { "subject": { "type": "string" } } } }
            } },
            { "id": "send", "kind": "tool_call", "name": "Send", "config": {
                "slug": "GMAIL_SEND_EMAIL",
                "args": { "subject": "=nodes.draft.item.json.subject" }
            } },
        ]),
        json!([{ "from_node": "t", "to_node": "draft" },
               { "from_node": "draft", "to_node": "send" }]),
    );

    assert!(
        unresolvable_tool_args(&g, &["oh:"]).await.is_empty(),
        "{:?}",
        unresolvable_tool_args(&g, &["oh:"]).await
    );
}

// ---- attributing an implicit `item` reference to one upstream node ----

/// `item` addresses the direct predecessor, so it can be attributed — but only
/// when there is exactly one. An ambiguous fan-in must return `None` rather
/// than blame whichever edge happened to be declared first.
#[test]
fn an_ambiguous_fan_in_is_not_attributed_to_a_single_upstream() {
    let g = graph(
        json!([
            { "id": "a", "kind": "tool_call", "name": "A", "config": { "slug": "X" } },
            { "id": "b", "kind": "tool_call", "name": "B", "config": { "slug": "Y" } },
            { "id": "sink", "kind": "tool_call", "name": "Sink", "config": { "slug": "Z" } },
        ]),
        json!([{ "from_node": "a", "to_node": "sink" },
               { "from_node": "b", "to_node": "sink" }]),
    );

    assert_eq!(
        mock_opaque_tool_call_upstream_ref("=item.json.data.x", &g, "sink"),
        None
    );
}

#[test]
fn a_sole_tool_call_predecessor_is_attributed_through_an_implicit_item() {
    let g = graph(
        json!([
            { "id": "a", "kind": "tool_call", "name": "A", "config": { "slug": "X" } },
            { "id": "sink", "kind": "tool_call", "name": "Sink", "config": { "slug": "Z" } },
        ]),
        json!([{ "from_node": "a", "to_node": "sink" }]),
    );

    assert_eq!(
        mock_opaque_tool_call_upstream_ref("=item.json.data.x", &g, "sink"),
        Some("a")
    );
}

/// Both addressing forms the engine can trace resolve to the same node.
#[test]
fn both_the_dotted_and_the_bracket_form_name_the_same_node() {
    let g = graph(
        json!([
            { "id": "a", "kind": "tool_call", "name": "A", "config": { "slug": "X" } },
            { "id": "sink", "kind": "tool_call", "name": "Sink", "config": { "slug": "Z" } },
        ]),
        json!([]),
    );

    assert_eq!(
        mock_opaque_tool_call_upstream_ref("=nodes.a.item.json.data.x", &g, "sink"),
        Some("a")
    );
    assert_eq!(
        mock_opaque_tool_call_upstream_ref("=.nodes[\"a\"].item.json.data.x", &g, "sink"),
        Some("a")
    );
}

/// Only a `tool_call` upstream is opaque. An `agent` or a `transform` really
/// does produce its output under the mocks, so a null read from one is a bug
/// worth reporting — which is exactly what returning `None` here allows.
#[test]
fn a_non_tool_call_upstream_is_not_treated_as_opaque() {
    let g = graph(
        json!([
            { "id": "a", "kind": "transform", "name": "A", "config": {} },
            { "id": "sink", "kind": "tool_call", "name": "Sink", "config": { "slug": "Z" } },
        ]),
        json!([{ "from_node": "a", "to_node": "sink" }]),
    );

    assert_eq!(
        mock_opaque_tool_call_upstream_ref("=item.x", &g, "sink"),
        None
    );
}
