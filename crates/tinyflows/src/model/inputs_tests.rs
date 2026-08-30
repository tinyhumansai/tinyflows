use super::*;
use serde_json::json;

fn supplied(pairs: &[(&str, Value)]) -> Map<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), v.clone()))
        .collect()
}

#[test]
fn input_type_defaults_to_string_and_round_trips() {
    let input: WorkflowInput = serde_json::from_str(r#"{"name":"note"}"#).unwrap();
    assert_eq!(input.ty, InputType::String);
    assert!(!input.required);
    assert_eq!(input.default, None);
    assert_eq!(input.description, None);

    let json = serde_json::to_value(&input).unwrap();
    assert_eq!(json["type"], "string");
    let back: WorkflowInput = serde_json::from_value(json).unwrap();
    assert_eq!(back, input);
}

#[test]
fn input_type_accepts_matching_json_only() {
    assert!(InputType::String.accepts(&json!("x")));
    assert!(!InputType::String.accepts(&json!(1)));
    assert!(InputType::Number.accepts(&json!(1.5)));
    assert!(!InputType::Number.accepts(&json!("1.5")));
    assert!(InputType::Boolean.accepts(&json!(true)));
    assert!(!InputType::Boolean.accepts(&json!("true")));
    for value in [json!(null), json!([1]), json!({"a": 1}), json!("s")] {
        assert!(InputType::Json.accepts(&value), "json rejected {value}");
    }
}

#[test]
fn valid_input_names() {
    for name in ["a", "_", "repo", "repo_url", "_x1", "A9"] {
        assert!(is_valid_input_name(name), "{name} should be valid");
    }
    for name in ["", "1a", "repo-url", "repo.url", "repo url", "café"] {
        assert!(!is_valid_input_name(name), "{name} should be invalid");
    }
}

#[test]
fn resolves_supplied_default_and_null() {
    let declared = vec![
        WorkflowInput::new("repo", InputType::String).required(),
        WorkflowInput::new("depth", InputType::Number).with_default(json!(3)),
        WorkflowInput::new("note", InputType::String),
    ];
    let resolved = resolve_inputs(&declared, &supplied(&[("repo", json!("acme/api"))])).unwrap();

    assert_eq!(resolved.len(), 3, "one entry per declared input");
    assert_eq!(resolved["repo"], json!("acme/api"));
    assert_eq!(resolved["depth"], json!(3));
    assert_eq!(resolved["note"], json!(null));
}

#[test]
fn supplied_value_overrides_default() {
    let declared = vec![WorkflowInput::new("depth", InputType::Number).with_default(json!(3))];
    let resolved = resolve_inputs(&declared, &supplied(&[("depth", json!(9))])).unwrap();
    assert_eq!(resolved["depth"], json!(9));
}

#[test]
fn missing_required_input_is_rejected() {
    let declared = vec![WorkflowInput::new("repo", InputType::String).required()];
    let err = resolve_inputs(&declared, &Map::new()).unwrap_err();
    assert_eq!(err, InputError::Missing("repo".into()));
    assert_eq!(err.code(), "input_missing");
    assert_eq!(err.input_name(), "repo");
    assert_eq!(
        err.to_string(),
        "workflow input \"repo\" is required but was not supplied"
    );
}

#[test]
fn type_mismatch_is_rejected_without_coercion() {
    let declared = vec![WorkflowInput::new("depth", InputType::Number)];
    let err = resolve_inputs(&declared, &supplied(&[("depth", json!("3"))])).unwrap_err();
    assert_eq!(
        err,
        InputError::TypeMismatch {
            name: "depth".into(),
            expected: "number",
            found: "string",
        }
    );
    assert_eq!(err.code(), "input_type_mismatch");
    assert_eq!(
        err.to_string(),
        "workflow input \"depth\" expects type number but received string"
    );
}

#[test]
fn explicit_null_fails_a_scalar_type_but_passes_json() {
    let scalar = vec![WorkflowInput::new("depth", InputType::Number)];
    assert!(resolve_inputs(&scalar, &supplied(&[("depth", json!(null))])).is_err());

    let any = vec![WorkflowInput::new("payload", InputType::Json)];
    let resolved = resolve_inputs(&any, &supplied(&[("payload", json!(null))])).unwrap();
    assert_eq!(resolved["payload"], json!(null));
}

#[test]
fn undeclared_key_is_rejected() {
    let declared = vec![WorkflowInput::new("repo", InputType::String)];
    let err = resolve_inputs(
        &declared,
        &supplied(&[("repo", json!("a")), ("reop", json!("typo"))]),
    )
    .unwrap_err();
    assert_eq!(err, InputError::Unknown("reop".into()));
    assert_eq!(err.code(), "input_unknown");
}

#[test]
fn declaration_errors_are_reported_before_undeclared_keys() {
    // A caller who both forgot a required input and mistyped another key
    // hears about the required one first — it is the actionable failure.
    let declared = vec![WorkflowInput::new("repo", InputType::String).required()];
    let err = resolve_inputs(&declared, &supplied(&[("reop", json!("typo"))])).unwrap_err();
    assert_eq!(err, InputError::Missing("repo".into()));
}

#[test]
fn no_declarations_accepts_nothing_and_yields_empty() {
    assert!(resolve_inputs(&[], &Map::new()).unwrap().is_empty());
    assert_eq!(
        resolve_inputs(&[], &supplied(&[("x", json!(1))])).unwrap_err(),
        InputError::Unknown("x".into())
    );
}

#[test]
fn builders_compose() {
    let input = WorkflowInput::new("depth", InputType::Number)
        .with_default(json!(3))
        .with_description("How deep to recurse");
    assert_eq!(input.default, Some(json!(3)));
    assert_eq!(input.description.as_deref(), Some("How deep to recurse"));
    assert!(!input.required);
    assert!(
        WorkflowInput::new("repo", InputType::String)
            .required()
            .required
    );
}
