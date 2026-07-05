//! Minimal JSON-Schema validation plus a one-shot LLM auto-fix.
//!
//! Shared by the [`output_parser`](super::output_parser) node and the
//! [`agent`](super::agent) node's output-parser sub-port. The validator covers a
//! deliberately small, dependency-free subset of JSON Schema — enough to give the
//! parser real teeth without pulling a heavyweight schema crate (which would also
//! not be a host capability, so it must stay in-crate and light):
//!
//! - `type` — one of `object` / `array` / `string` / `number` / `integer` /
//!   `boolean` / `null`, or an array of those (any-of);
//! - `required` — required property names on an object;
//! - `properties` — per-property subschemas (recursed when the property exists);
//! - `items` — a subschema applied to every array element;
//! - `enum` — an explicit set of allowed values.
//!
//! Unknown keywords are ignored (they never fail validation), and a non-object
//! schema (e.g. `true`) accepts anything. Anything richer — `$ref`, `oneOf`,
//! numeric bounds, string patterns — is a documented follow-up.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::caps::LlmProvider;
use crate::error::{EngineError, Result};

/// Validates `value` against the supported subset of JSON Schema, returning a
/// list of human-readable error messages. An empty list means the value is
/// valid.
#[must_use]
pub(crate) fn validate(value: &Value, schema: &Value) -> Vec<String> {
    let mut errors = Vec::new();
    validate_at("$", value, schema, &mut errors);
    errors
}

/// Recursively validates `value` at JSON-path `path` against `schema`, appending
/// any failures to `errors`.
fn validate_at(path: &str, value: &Value, schema: &Value, errors: &mut Vec<String>) {
    // A non-object schema (e.g. the boolean `true`) accepts anything.
    let Some(obj) = schema.as_object() else {
        return;
    };

    // `enum`: the value must be one of the listed values (by JSON equality).
    if let Some(Value::Array(allowed)) = obj.get("enum") {
        if !allowed.iter().any(|a| a == value) {
            errors.push(format!(
                "{path}: value is not one of the allowed `enum` values"
            ));
        }
    }

    // `type`: a single type name or an array of acceptable type names.
    if let Some(ty) = obj.get("type") {
        let types: Vec<&str> = match ty {
            Value::String(s) => vec![s.as_str()],
            Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
            _ => vec![],
        };
        if !types.is_empty() && !types.iter().any(|t| type_matches(t, value)) {
            errors.push(format!(
                "{path}: expected type {types:?}, got `{}`",
                type_name(value)
            ));
            // The value is the wrong shape; deeper structural checks below would
            // only produce noise, so stop here for this node.
            return;
        }
    }

    // Object constraints: required properties and per-property subschemas.
    if let Value::Object(map) = value {
        if let Some(Value::Array(required)) = obj.get("required") {
            for name in required.iter().filter_map(Value::as_str) {
                if !map.contains_key(name) {
                    errors.push(format!("{path}: missing required property `{name}`"));
                }
            }
        }
        if let Some(Value::Object(props)) = obj.get("properties") {
            for (key, subschema) in props {
                if let Some(child) = map.get(key) {
                    validate_at(&format!("{path}.{key}"), child, subschema, errors);
                }
            }
        }
    }

    // Array constraints: a single `items` subschema applied to each element.
    if let Value::Array(arr) = value {
        if let Some(items_schema) = obj.get("items") {
            for (i, child) in arr.iter().enumerate() {
                validate_at(&format!("{path}[{i}]"), child, items_schema, errors);
            }
        }
    }
}

/// Whether `value` satisfies the JSON-Schema `type` name `ty`. An unknown type
/// keyword is treated as satisfied (we never fail on keywords we don't model).
fn type_matches(ty: &str, value: &Value) -> bool {
    match ty {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "number" => value.is_number(),
        // JSON Schema treats `1.0` as an integer; accept whole-valued floats too.
        "integer" => {
            value.is_i64() || value.is_u64() || value.as_f64().is_some_and(|f| f.fract() == 0.0)
        }
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => true,
    }
}

/// The JSON type name of `value`, for error messages.
fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Validates `value` against `schema`; on failure, optionally makes a single LLM
/// auto-fix attempt and re-validates.
///
/// Returns the validated (possibly repaired) value on success. When validation
/// fails and `auto_fix` is set, the injected [`LlmProvider`] is asked once to
/// coerce the value to the schema — the request carries the `schema`, the
/// offending `value`, and the validation `errors`, and the corrected value is
/// read back from the response's `value` field (or the whole response when it has
/// no such field). If it still fails to validate — or `auto_fix` is off — a
/// [`EngineError::Capability`] describing the failures is returned, which the
/// engine then routes per the node's `on_error` policy.
pub(crate) async fn parse_and_validate(
    value: Value,
    schema: &Value,
    auto_fix: bool,
    llm: &Arc<dyn LlmProvider>,
    conn: Option<&str>,
) -> Result<Value> {
    let errors = validate(&value, schema);
    if errors.is_empty() {
        return Ok(value);
    }
    if !auto_fix {
        return Err(EngineError::Capability(format!(
            "output_parser: value failed schema validation: {}",
            errors.join("; ")
        )));
    }

    tracing::debug!(
        error_count = errors.len(),
        "output_parser: schema validation failed; attempting one LLM auto-fix"
    );

    let request = json!({
        "task": "coerce_to_schema",
        "schema": schema,
        "value": value,
        "errors": errors,
    });
    let response = llm.complete(request, conn).await?;
    // The corrected value is the response's `value` field when present, else the
    // whole response body.
    let fixed = response
        .get("value")
        .cloned()
        .unwrap_or_else(|| response.clone());

    let remaining = validate(&fixed, schema);
    if remaining.is_empty() {
        tracing::debug!("output_parser: LLM auto-fix produced a schema-valid value");
        Ok(fixed)
    } else {
        Err(EngineError::Capability(format!(
            "output_parser: value failed schema validation after auto-fix: {}",
            remaining.join("; ")
        )))
    }
}

#[cfg(test)]
mod tests {
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
}
