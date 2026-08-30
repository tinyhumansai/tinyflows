//! Synthesizing a value that satisfies a declared output schema.
//!
//! A node that declares an `output_parser.schema` says what its result must
//! look like. Anything standing in for that node's real capability — a dry-run
//! mock, a test double, an auto-mocked `testkit` run — has to produce something
//! that shape, or the graph fails validation for a reason that has nothing to
//! do with the graph.
//!
//! This lives in [`crate::caps`] rather than beside any one set of mocks
//! because more than one of them needs it and none of them should have to pull
//! in a process runner or an HTTP client to get it.

use serde_json::{Value, json};

/// Synthesize a value satisfying a JSON Schema well enough to pass validation.
///
/// Deliberately shallow — it honours `type`, `properties`, `required`, and
/// `enum`, which is what node schemas in practice use. Anything it does not
/// understand becomes null, and a schema strict enough to reject that is a
/// schema whose graph deserves a real run before being trusted.
///
/// ```
/// use tinyflows::caps::sample_for_schema;
/// use serde_json::json;
///
/// let sample = sample_for_schema(&json!({
///     "type": "object",
///     "properties": { "name": { "type": "string" }, "count": { "type": "integer" } }
/// }));
/// assert_eq!(sample, json!({ "name": "sample", "count": 0 }));
/// ```
#[must_use]
pub fn sample_for_schema(schema: &Value) -> Value {
    let Some(object) = schema.as_object() else {
        return Value::Null;
    };
    if let Some(first) = object
        .get("enum")
        .and_then(Value::as_array)
        .and_then(|v| v.first())
    {
        return first.clone();
    }
    match object.get("type").and_then(Value::as_str) {
        Some("object") => {
            let mut out = serde_json::Map::new();
            if let Some(properties) = object.get("properties").and_then(Value::as_object) {
                // Every declared property, not only the required ones: a graph
                // binding `=item.json.optional_field` should still resolve.
                for (name, property) in properties {
                    out.insert(name.clone(), sample_for_schema(property));
                }
            }
            Value::Object(out)
        }
        Some("array") => match object.get("items") {
            // One element, so a downstream `per_item` node has something to map
            // over and a `[0]` expression resolves.
            Some(items) => json!([sample_for_schema(items)]),
            None => json!([]),
        },
        Some("string") => json!("sample"),
        Some("integer") | Some("number") => json!(0),
        Some("boolean") => json!(false),
        _ => Value::Null,
    }
}
