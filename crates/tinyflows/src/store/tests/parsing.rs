//! Document parsing: id fallback, schema migration, the `defaults` block, and
//! structural validation reporting every failure rather than only the first.

use serde_json::json;

use super::*;

#[test]
fn parsing_names_a_workflow_by_its_filename_when_the_document_omits_an_id() {
    let document = json!({
        "nodes": [{ "id": "t", "kind": "trigger", "name": "start" }],
        "edges": []
    })
    .to_string();

    let record = parse_workflow(&document, "nightly-sweep").expect("parses");

    assert_eq!(record.id, "nightly-sweep");
    // Name falls back to the id so a listing is never blank.
    assert_eq!(record.name, "nightly-sweep");
    assert!(record.enabled, "workflows are enabled unless opted out");
}

#[test]
fn parsing_migrates_a_document_saved_without_a_schema_version() {
    // A document predating the field must keep loading; the engine's migration
    // runs before deserialization, not after.
    let document = json!({
        "id": "old",
        "nodes": [{ "id": "t", "kind": "trigger", "name": "start" }],
        "edges": []
    })
    .to_string();

    let record = parse_workflow(&document, "old").expect("parses");

    assert_eq!(
        record.graph.schema_version,
        crate::model::CURRENT_SCHEMA_VERSION
    );
}

#[test]
fn parsing_reads_the_defaults_block() {
    let document = json!({
        "id": "nightly",
        "defaults": { "harness": "codex", "model": "gpt-5-codex" },
        "nodes": [{ "id": "t", "kind": "trigger", "name": "start" }],
        "edges": []
    })
    .to_string();

    let record = parse_workflow(&document, "nightly").expect("parses");

    assert_eq!(record.defaults.harness.as_deref(), Some("codex"));
    assert_eq!(record.defaults.model.as_deref(), Some("gpt-5-codex"));
}

#[test]
fn a_host_policy_can_refuse_a_defaults_block_the_engine_cannot_judge() {
    // Which harnesses exist is the host's vocabulary, not the engine's, so the
    // rule is injected. What matters here is *when* it runs: on the way in
    // rather than at dispatch, because a workflow that meant to change where its
    // work runs and quietly ran it on the host default is the failure this
    // exists to prevent.
    #[derive(Debug)]
    struct OnlyKnownHarnesses;

    impl HostPolicy for OnlyKnownHarnesses {
        fn check_defaults(&self, defaults: &WorkflowDefaults) -> Result<(), String> {
            match defaults.harness.as_deref() {
                Some(name) if name != "codex" => Err(format!("no harness named '{name}'")),
                _ => Ok(()),
            }
        }
    }

    let document = json!({
        "id": "nightly",
        "defaults": { "harness": "claude code" },
        "nodes": [{ "id": "t", "kind": "trigger", "name": "start" }],
        "edges": []
    })
    .to_string();

    let err = parse_workflow_with(&document, "nightly", &OnlyKnownHarnesses).expect_err("refused");

    assert!(err.contains("defaults"), "{err}");
    assert!(err.contains("claude code"), "{err}");
}

#[test]
fn with_no_host_policy_an_unknown_harness_name_is_carried_through() {
    // The engine has no opinion about the string, so the default is to keep it
    // rather than to guess. A host that cares supplies a policy.
    let document = json!({
        "id": "nightly",
        "defaults": { "harness": "something-else" },
        "nodes": [{ "id": "t", "kind": "trigger", "name": "start" }],
        "edges": []
    })
    .to_string();

    let record = parse_workflow(&document, "nightly").expect("parses");

    assert_eq!(record.defaults.harness.as_deref(), Some("something-else"));
}

#[test]
fn a_document_without_defaults_stays_without_them() {
    let record = parse_workflow(&valid_document("plain"), "plain").expect("parses");
    assert!(record.defaults.is_empty());
}

#[test]
fn parsing_rejects_a_document_that_is_not_an_object() {
    let err = parse_workflow("[]", "list").expect_err("an array is not a workflow");
    assert!(err.contains("object"), "unhelpful message: {err}");
}

#[test]
fn validation_reports_every_failure_not_only_the_first() {
    // A graph with no trigger *and* an edge to a node that does not exist. An
    // author — often an agent editing over a tool call — should learn both in
    // one round-trip.
    let graph = serde_json::from_value(json!({
        "nodes": [{ "id": "a", "kind": "transform", "name": "a" }],
        "edges": [{ "from_node": "a", "to_node": "ghost" }]
    }))
    .unwrap();

    let err = validate_graph("broken", &graph).expect_err("invalid");
    let WorkflowError::Invalid { messages, .. } = err else {
        panic!("expected Invalid, got {err:?}");
    };

    assert!(
        messages.len() >= 2,
        "expected every failure, got {messages:?}"
    );
    assert!(
        messages.iter().any(|m| m.contains("missing_trigger")),
        "missing trigger not reported: {messages:?}"
    );
}
