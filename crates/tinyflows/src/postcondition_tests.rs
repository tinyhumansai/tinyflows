use super::*;
use serde_json::json;

fn require(require: &str) -> Postcondition {
    Postcondition {
        require: require.to_string(),
        field: None,
    }
}

fn require_field(require: &str, field: &str) -> Postcondition {
    Postcondition {
        require: require.to_string(),
        field: Some(field.to_string()),
    }
}

// --- `non_empty` ---

#[test]
fn non_empty_passes_on_real_text() {
    let output = json!({ "json": null, "text": "the report is done", "raw": "" });
    assert_eq!(require("non_empty").check(&output), Ok(()));
}

#[test]
fn non_empty_fails_on_blank_text() {
    let gap = require("non_empty")
        .check(&json!({ "text": "  \n " }))
        .unwrap_err();
    assert!(gap.contains("empty"), "{gap}");
}

#[test]
fn non_empty_fails_on_missing_or_null_text() {
    assert!(require("non_empty").check(&json!({ "json": {} })).is_err());
    assert!(
        require("non_empty")
            .check(&json!({ "text": null }))
            .is_err()
    );
}

// --- `field_present` ---

#[test]
fn field_present_passes_when_a_dotted_path_resolves() {
    let output = json!({ "json": { "result": { "count": 0 } } });
    assert_eq!(
        require_field("field_present", "json.result.count").check(&output),
        Ok(())
    );
}

#[test]
fn field_present_fails_when_the_path_breaks_partway() {
    let output = json!({ "json": { "result": {} } });
    let gap = require_field("field_present", "json.result.count")
        .check(&output)
        .unwrap_err();
    assert!(gap.contains("`json.result.count`"), "{gap}");
}

#[test]
fn field_present_fails_on_an_explicit_null() {
    let output = json!({ "json": null, "text": "prose" });
    assert!(
        require_field("field_present", "json")
            .check(&output)
            .is_err()
    );
}

#[test]
fn field_present_does_not_index_into_arrays() {
    let output = json!({ "json": [{ "id": 1 }] });
    assert!(
        require_field("field_present", "json.0.id")
            .check(&output)
            .is_err()
    );
}

/// `field_present` exists to check one named field. Handed none, it has
/// nothing to verify — passing would silently switch the gate off.
#[test]
fn field_present_without_a_field_fails_closed() {
    let gap = require("field_present")
        .check(&json!({ "json": { "a": 1 } }))
        .unwrap_err();
    assert!(gap.contains("field"), "{gap}");
    assert!(
        require_field("field_present", "")
            .check(&json!({ "": 1 }))
            .is_err()
    );
}

// --- `non_empty_list` ---

#[test]
fn non_empty_list_defaults_to_the_structured_payload() {
    let output = json!({ "json": [1, 2], "text": null, "raw": [1, 2] });
    assert_eq!(require("non_empty_list").check(&output), Ok(()));
}

#[test]
fn non_empty_list_falls_back_to_the_whole_value_without_an_envelope() {
    assert_eq!(require("non_empty_list").check(&json!(["a"])), Ok(()));
}

#[test]
fn non_empty_list_names_an_empty_list() {
    let gap = require("non_empty_list")
        .check(&json!({ "json": [] }))
        .unwrap_err();
    assert!(gap.contains("empty list"), "{gap}");
}

#[test]
fn non_empty_list_names_a_shape_mismatch() {
    let gap = require_field("non_empty_list", "json.items")
        .check(&json!({ "json": { "items": { "a": 1 } } }))
        .unwrap_err();
    assert!(gap.contains("`json.items` is not a list"), "{gap}");
}

#[test]
fn non_empty_list_names_a_missing_field() {
    let gap = require_field("non_empty_list", "json.items")
        .check(&json!({ "json": {} }))
        .unwrap_err();
    assert!(gap.contains("`json.items` is missing"), "{gap}");
}

#[test]
fn non_empty_list_passes_on_a_field() {
    let output = json!({ "json": { "items": [{}] } });
    assert_eq!(
        require_field("non_empty_list", "json.items").check(&output),
        Ok(())
    );
}

// --- an unrecognised predicate ---

/// A declared gate this build cannot evaluate must not be skipped: that
/// would delete the author's quality check while the run reads as healthy.
#[test]
fn an_unknown_requirement_fails_closed_and_names_itself() {
    let gap = require("non_trivial").check(&json!({})).unwrap_err();
    assert!(gap.contains("`non_trivial`"), "{gap}");
}

// --- over a node's emitted items ---

#[test]
fn every_item_must_clear_the_gate() {
    let gate = require("non_empty");
    let good = Item::new(json!({ "text": "a" }));
    let bad = Item::new(json!({ "text": "" }));
    assert_eq!(gate.check_items(&[good.clone(), good.clone()]), Ok(()));
    let gap = gate.check_items(&[good, bad]).unwrap_err();
    assert!(gap.contains("item 1"), "{gap}");
}

/// A node that emitted nothing has no output with the declared shape.
#[test]
fn an_empty_emission_fails_every_requirement() {
    for name in REQUIREMENTS {
        assert!(require_field(name, "json").check_items(&[]).is_err());
    }
}

// --- reading the declaration off node config ---

#[test]
fn a_node_without_a_postcondition_declares_nothing() {
    assert!(Postcondition::from_config(&json!({})).is_none());
    assert!(Postcondition::from_config(&Value::Null).is_none());
}

#[test]
fn a_declared_postcondition_reads_off_config() {
    let read = Postcondition::from_config(&json!({
        "postcondition": { "require": "field_present", "field": "json.id" }
    }));
    assert_eq!(read, Some(Ok(require_field("field_present", "json.id"))));
}

#[test]
fn a_malformed_declaration_is_an_error_not_an_absence() {
    let read = Postcondition::from_config(&json!({ "postcondition": "non_empty" }));
    assert!(matches!(read, Some(Err(_))), "{read:?}");
}

#[test]
fn the_wire_shape_omits_an_absent_field() {
    assert_eq!(
        serde_json::to_value(require("non_empty")).unwrap(),
        json!({ "require": "non_empty" })
    );
    assert_eq!(
        serde_json::to_value(require_field("field_present", "json.a")).unwrap(),
        json!({ "require": "field_present", "field": "json.a" })
    );
}

// --- author-time validation ---

#[test]
fn validate_accepts_every_well_formed_declaration() {
    assert_eq!(require("non_empty").validate(), Ok(()));
    assert_eq!(require("non_empty_list").validate(), Ok(()));
    assert_eq!(
        require_field("non_empty_list", "json.items").validate(),
        Ok(())
    );
    assert_eq!(require_field("field_present", "json.id").validate(), Ok(()));
}

#[test]
fn validate_rejects_an_unknown_requirement() {
    let reason = require("nonempty").validate().unwrap_err();
    assert!(
        reason.contains("non_empty, field_present, non_empty_list"),
        "{reason}"
    );
}

#[test]
fn validate_requires_a_field_for_field_present() {
    assert!(require("field_present").validate().is_err());
    assert!(require_field("field_present", " ").validate().is_err());
}

/// The engine reads the declaration raw — it is never expression-resolved —
/// so an `=`-expression would be checked as a literal key that can never
/// exist. Refuse it where the author can see why.
#[test]
fn validate_rejects_an_expression_as_the_field() {
    let reason = require_field("field_present", "=item.json.id")
        .validate()
        .unwrap_err();
    assert!(reason.contains("dotted path"), "{reason}");
}

#[test]
fn validate_rejects_a_field_with_an_empty_segment() {
    assert!(
        require_field("field_present", "json..id")
            .validate()
            .is_err()
    );
    assert!(require_field("non_empty_list", "json.").validate().is_err());
}

// --- the engine's side of the gate ---

fn node_with(config: Value) -> Node {
    Node {
        id: "x".to_string(),
        kind: crate::model::NodeKind::Agent,
        type_version: 1,
        name: "x".to_string(),
        config,
        ports: Vec::new(),
        position: None,
    }
}

/// A pause, re-entry or fan-out is not the node's settled answer; checking
/// it would fail every gated node that waits.
#[test]
fn an_output_carrying_control_is_not_checked() {
    let node = node_with(json!({ "postcondition": { "require": "non_empty" } }));
    let pausing = NodeOutput {
        control: Some(crate::nodes::NodeControl::Reenter { after_ms: 10 }),
        ..NodeOutput::empty()
    };
    assert!(enforce(&node, &pausing).is_ok());
    assert!(enforce(&node, &NodeOutput::empty()).is_err());
}

#[test]
fn a_malformed_declaration_fails_the_attempt() {
    let node = node_with(json!({ "postcondition": ["non_empty"] }));
    let output = NodeOutput::main(vec![Item::new(json!({ "text": "fine" }))]);
    let err = enforce(&node, &output).unwrap_err().to_string();
    assert!(err.contains("malformed"), "{err}");
}

#[test]
fn an_undeclared_gate_passes_anything() {
    assert!(enforce(&node_with(json!({})), &NodeOutput::empty()).is_ok());
}
