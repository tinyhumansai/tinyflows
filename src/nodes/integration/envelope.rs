//! The stable output envelope for capability-backed nodes.
//!
//! Every capability node (`agent`, `tool_call`, `http_request`, `code`) returns
//! a provider-native `Value`. Passing that through verbatim means the shape a
//! downstream node reads depends on the provider (and, for the agent, on which
//! sub-ports fired and even on what the model emitted at runtime), so
//! `=item.<field>` expressions can only guess.
//!
//! To give consumers a guaranteed contract, capability nodes wrap their result
//! in a fixed envelope:
//!
//! ```jsonc
//! {
//!   "json": <structured payload | null>,  // objects/arrays; addressable via =item.json.<field>
//!   "text": <human-readable string | null>, // the model's prose; addressable via =item.text
//!   "raw":  <the untouched capability return> // escape hatch / provenance
//! }
//! ```
//!
//! `=item.text` and `=item.json` therefore resolve predictably regardless of
//! provider, and `=item.raw` preserves the pre-envelope behavior for callers
//! that need the exact provider payload.

use serde_json::{Value, json};

/// Extracts a human-readable string from a capability value: the value itself
/// when it is a string, else its `text` field when that is a string, else
/// `None`.
// `wrap`/`text_of`/`structured_of` back the pure capability nodes (tool_call,
// http_request, code); the `agent` node uses `from_parts` directly. Until those
// nodes adopt the envelope they are constructed-but-unused here.
#[allow(dead_code)]
fn text_of(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Object(map) => map.get("text").and_then(Value::as_str).map(str::to_string),
        _ => None,
    }
}

/// The structured payload of a capability value: the value itself when it is an
/// object or array, else [`Value::Null`] (scalars carry no structure).
#[allow(dead_code)]
fn structured_of(value: &Value) -> Value {
    match value {
        Value::Object(_) | Value::Array(_) => value.clone(),
        _ => Value::Null,
    }
}

/// Assembles the envelope from explicit parts. Used when the structured payload
/// differs from `raw` — e.g. the `agent` node whose `json` is the
/// schema-coerced / tool-augmented value while `text`/`raw` come from the
/// original completion.
#[must_use]
pub(crate) fn from_parts(json: Value, text: Option<String>, raw: Value) -> Value {
    json!({ "json": json, "text": text, "raw": raw })
}

/// Wraps a capability's return `value` in the stable envelope, deriving `json`
/// and `text` from it. Used by the pure capability nodes (`tool_call`,
/// `http_request`, `code`) whose structured payload *is* the raw return.
#[allow(dead_code)]
#[must_use]
pub(crate) fn wrap(value: Value) -> Value {
    let text = text_of(&value);
    let json = structured_of(&value);
    from_parts(json, text, value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_a_structured_object() {
        let env = wrap(json!({ "answer": 42 }));
        assert_eq!(env["json"], json!({ "answer": 42 }));
        assert_eq!(env["text"], Value::Null);
        assert_eq!(env["raw"], json!({ "answer": 42 }));
    }

    #[test]
    fn wraps_a_bare_string_as_text() {
        let env = wrap(json!("hello world"));
        assert_eq!(env["json"], Value::Null);
        assert_eq!(env["text"], json!("hello world"));
        assert_eq!(env["raw"], json!("hello world"));
    }

    #[test]
    fn extracts_text_field_from_an_object() {
        let env = wrap(json!({ "text": "hi", "meta": 1 }));
        // Both accessors are available: the prose via `text`, the object via `json`.
        assert_eq!(env["text"], json!("hi"));
        assert_eq!(env["json"], json!({ "text": "hi", "meta": 1 }));
    }

    #[test]
    fn scalar_non_string_has_null_json_and_text() {
        let env = wrap(json!(7));
        assert_eq!(env["json"], Value::Null);
        assert_eq!(env["text"], Value::Null);
        assert_eq!(env["raw"], json!(7));
    }

    #[test]
    fn from_parts_keeps_structured_and_raw_distinct() {
        // Agent case: json is the coerced value, raw is the original completion.
        let env = from_parts(
            json!({ "name": "fixed" }),
            Some("original prose".into()),
            json!({ "wrong": 1 }),
        );
        assert_eq!(env["json"], json!({ "name": "fixed" }));
        assert_eq!(env["text"], json!("original prose"));
        assert_eq!(env["raw"], json!({ "wrong": 1 }));
    }

    #[test]
    fn text_key_is_always_present_even_when_null() {
        let env = wrap(json!({ "x": 1 }));
        assert!(env.as_object().unwrap().contains_key("text"));
        assert_eq!(env["text"], Value::Null);
    }
}
