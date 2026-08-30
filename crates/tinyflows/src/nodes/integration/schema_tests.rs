use super::*;
use async_trait::async_trait;

#[test]
fn no_schema_object_accepts_anything() {
    // A boolean/non-object schema imposes no constraints.
    assert!(validate(&json!({"a": 1}), &Value::Bool(true)).is_empty());
}

#[test]
fn type_and_required_and_properties() {
    let schema = json!({
        "type": "object",
        "required": ["name", "age"],
        "properties": {
            "name": { "type": "string" },
            "age": { "type": "integer" }
        }
    });
    assert!(validate(&json!({"name": "A", "age": 3}), &schema).is_empty());

    let missing = validate(&json!({"name": "A"}), &schema);
    assert_eq!(missing.len(), 1);
    assert!(missing[0].contains("missing required property `age`"));

    let wrong_type = validate(&json!({"name": "A", "age": "old"}), &schema);
    assert!(wrong_type.iter().any(|e| e.contains("expected type")));
}

#[test]
fn integer_accepts_whole_floats() {
    let schema = json!({ "type": "integer" });
    assert!(validate(&json!(3.0), &schema).is_empty());
    assert!(!validate(&json!(3.5), &schema).is_empty());
}

#[test]
fn enum_and_array_items() {
    let schema = json!({
        "type": "array",
        "items": { "enum": ["a", "b"] }
    });
    assert!(validate(&json!(["a", "b", "a"]), &schema).is_empty());
    assert!(!validate(&json!(["a", "z"]), &schema).is_empty());
}

/// An LLM that returns a fixed value under `value` on the auto-fix call.
struct FixingLlm(Value);

#[async_trait]
impl LlmProvider for FixingLlm {
    async fn complete(&self, _request: Value, _conn: Option<&str>) -> Result<Value> {
        Ok(json!({ "value": self.0.clone() }))
    }
}

/// An LLM whose "fix" is still invalid.
struct UselessLlm;

#[async_trait]
impl LlmProvider for UselessLlm {
    async fn complete(&self, _request: Value, _conn: Option<&str>) -> Result<Value> {
        Ok(json!({ "value": { "still": "wrong" } }))
    }
}

#[tokio::test]
async fn valid_value_passes_without_calling_llm() {
    let schema = json!({ "type": "object", "required": ["ok"] });
    let llm: Arc<dyn LlmProvider> = Arc::new(UselessLlm);
    let out = parse_and_validate(json!({ "ok": true }), &schema, true, &llm, None)
        .await
        .expect("valid value passes");
    assert_eq!(out, json!({ "ok": true }));
}

#[tokio::test]
async fn invalid_value_is_repaired_by_auto_fix() {
    let schema = json!({ "type": "object", "required": ["name"] });
    let llm: Arc<dyn LlmProvider> = Arc::new(FixingLlm(json!({ "name": "fixed" })));
    let out = parse_and_validate(json!({ "wrong": 1 }), &schema, true, &llm, None)
        .await
        .expect("auto-fix repairs the value");
    assert_eq!(out, json!({ "name": "fixed" }));
}

#[tokio::test]
async fn unfixable_value_errors() {
    let schema = json!({ "type": "object", "required": ["name"] });
    let llm: Arc<dyn LlmProvider> = Arc::new(UselessLlm);
    let err = parse_and_validate(json!({ "wrong": 1 }), &schema, true, &llm, None)
        .await
        .expect_err("unfixable value must error");
    assert!(matches!(err, EngineError::Capability(ref m) if m.contains("after auto-fix")));
}

#[tokio::test]
async fn auto_fix_disabled_errors_immediately() {
    let schema = json!({ "type": "object", "required": ["name"] });
    let llm: Arc<dyn LlmProvider> = Arc::new(FixingLlm(json!({ "name": "fixed" })));
    let err = parse_and_validate(json!({ "wrong": 1 }), &schema, false, &llm, None)
        .await
        .expect_err("auto-fix disabled must error");
    assert!(
        matches!(err, EngineError::Capability(ref m) if m.contains("failed schema validation"))
    );
}
