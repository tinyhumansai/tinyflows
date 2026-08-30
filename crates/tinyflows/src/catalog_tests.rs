use super::*;
use crate::model::NodeKind;

#[test]
fn every_node_kind_has_a_contract() {
    for kind in NODE_KINDS {
        let c = contract_for(kind).unwrap_or_else(|| panic!("no contract for {kind}"));
        assert_eq!(c.kind, kind);
        assert!(!c.summary.is_empty(), "{kind} has empty summary");
        assert!(!c.description.is_empty(), "{kind} has empty description");
        assert_eq!(
            c.example.get("kind").and_then(Value::as_str),
            Some(kind),
            "{kind} example has the wrong kind"
        );
        for f in &c.config_fields {
            if f.value_type == "enum" {
                assert!(
                    f.enum_values.is_some(),
                    "{kind}.{} is an enum but lists no values",
                    f.name
                );
            }
        }
    }
    assert_eq!(all_contracts().len(), 22);
}

#[test]
fn node_kinds_match_the_model_enum() {
    // Every catalog entry must deserialize back to a real NodeKind, and the
    // count must match — a new NodeKind without a contract fails here.
    for kind in NODE_KINDS {
        let parsed: NodeKind = serde_json::from_value(Value::String(kind.to_string()))
            .unwrap_or_else(|_| panic!("catalog kind {kind} is not a real NodeKind discriminator"));
        // round-trips back to the same string
        assert_eq!(
            serde_json::to_value(parsed).unwrap(),
            Value::String(kind.to_string())
        );
    }
}

#[test]
fn unknown_kind_has_no_contract() {
    assert!(contract_for("not_a_kind").is_none());
    assert!(contract_for("").is_none());
}

#[test]
fn node_kinds_has_22_entries_including_the_async_and_lane_pairs() {
    assert_eq!(NODE_KINDS.len(), 22);
    assert!(NODE_KINDS.contains(&"shell"));
    assert!(NODE_KINDS.contains(&"memory"));
    assert!(NODE_KINDS.contains(&"dedup"));
    assert!(NODE_KINDS.contains(&"loop"));
    assert!(NODE_KINDS.contains(&"spawn"));
    assert!(NODE_KINDS.contains(&"gate"));
    // New kinds are appended, never inserted: a host that pins a position
    // (or renders the list in order) must not have entries shift under it.
    assert_eq!(NODE_KINDS[13], "memory");
    assert_eq!(NODE_KINDS[14], "dedup");
    assert_eq!(NODE_KINDS[15], "loop");
    assert!(NODE_KINDS.contains(&"scatter"));
    assert!(NODE_KINDS.contains(&"gather"));
    assert_eq!(NODE_KINDS[16], "spawn");
    assert_eq!(NODE_KINDS[17], "gate");
    assert_eq!(NODE_KINDS[18], "scatter");
    assert_eq!(NODE_KINDS[19], "gather");
    assert_eq!(NODE_KINDS[20], "approval");
    assert!(NODE_KINDS.contains(&"void"));
    assert_eq!(NODE_KINDS[21], "void");
}

#[test]
fn void_contract_takes_no_config_and_declares_no_output_port() {
    // The two claims an authoring tool acts on: there is nothing to configure,
    // and there is nowhere to draw an edge to. Both are enforced by validation,
    // so the contract must not suggest otherwise.
    let c = contract_for("void").expect("void contract exists");
    assert!(
        c.config_fields.is_empty(),
        "void takes no config; the reason goes in the node's name"
    );
    assert_eq!(c.ports, PortSpec::new(&["main"], &[]));
    assert!(
        c.notes.iter().any(|n| n.contains("outgoing edge")),
        "the contract must say an outgoing edge is refused"
    );
    assert!(
        c.notes.iter().any(|n| n.contains("spawn -> void")),
        "the contract must document the ungathered-ticket spelling"
    );
}

#[test]
fn memory_contract_documents_the_six_operations_and_scope_enum() {
    let c = contract_for("memory").expect("memory contract exists");
    let operation_field = c
        .config_fields
        .iter()
        .find(|f| f.name == "operation")
        .expect("memory contract declares `operation`");
    assert!(operation_field.required);
    assert_eq!(
        operation_field.enum_values,
        Some(
            vec![
                "recall", "search", "flavour", "people", "remember", "forget"
            ]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>()
        )
    );
    let scope_field = c
        .config_fields
        .iter()
        .find(|f| f.name == "scope")
        .expect("memory contract declares `scope`");
    assert_eq!(
        scope_field.enum_values,
        Some(
            vec!["user", "flow", "flows"]
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>()
        )
    );
    assert!(
        c.notes.iter().any(|n| n.contains("HARD SECURITY RULE")),
        "memory contract must document the user-scope write rejection"
    );
}

#[test]
fn dedup_contract_requires_key_and_documents_fail_open_behavior() {
    let c = contract_for("dedup").expect("dedup contract exists");
    let key_field = c
        .config_fields
        .iter()
        .find(|f| f.name == "key")
        .expect("dedup contract declares `key`");
    assert!(key_field.required);
    assert_eq!(key_field.value_type, "\"=expr\"");
    assert!(
        c.notes.iter().any(|n| n.contains("fail-open")),
        "dedup contract must document the null-key fail-open behavior"
    );
    assert_eq!(c.ports, PortSpec::linear());
}

#[test]
fn with_note_appends_a_host_caveat() {
    let c = contract_for("tool_call").unwrap().with_note("host says hi");
    assert_eq!(c.notes.last().map(String::as_str), Some("host says hi"));
}

#[test]
fn contracts_are_serde_round_trippable() {
    for c in all_contracts() {
        let json = serde_json::to_value(&c).unwrap();
        let back: NodeKindContract = serde_json::from_value(json).unwrap();
        assert_eq!(c, back);
    }
}
