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
fn text_of(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Object(map) => map.get("text").and_then(Value::as_str).map(str::to_string),
        _ => None,
    }
}

/// The structured payload of a capability value: the value itself when it is an
/// object or array, else [`Value::Null`] (scalars carry no structure).
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

/// Like [`from_parts`], plus a `meta` key carrying per-node execution facts the
/// payload itself cannot express.
///
/// Used by the `agent` node to publish *how* a run ended — `meta.stop` is
/// `"finished"`, `"limit_stop"`, or `"paused"` — so a downstream `condition` can
/// branch on whether the agent actually reached an answer rather than assuming
/// it did. Purely additive: `json` / `text` / `raw` are byte-identical to
/// [`from_parts`], so every existing `=item.json.…` binding is unaffected.
#[must_use]
pub(crate) fn from_parts_with_meta(
    json: Value,
    text: Option<String>,
    raw: Value,
    meta: Value,
) -> Value {
    json!({ "json": json, "text": text, "raw": raw, "meta": meta })
}

/// Wraps a capability's return `value` in the stable envelope, deriving `json`
/// and `text` from it. Used by the pure capability nodes (`tool_call`,
/// `http_request`, `code`) whose structured payload *is* the raw return.
#[must_use]
pub(crate) fn wrap(value: Value) -> Value {
    let text = text_of(&value);
    let json = structured_of(&value);
    from_parts(json, text, value)
}

#[cfg(test)]
#[path = "envelope_tests.rs"]
mod tests;
