use serde_json::{Map, Value};

pub(super) fn contains_expression_string(value: &Value) -> bool {
    match value {
        Value::String(text) => text.starts_with('=') && !text.starts_with("=.item"),
        Value::Array(items) => items.iter().any(contains_expression_string),
        Value::Object(map) => map.values().any(contains_expression_string),
        _ => false,
    }
}

/// Converts n8n's `{parameters:[{name,value}]}` collection to the object shape
/// tinyflows uses for HTTP bodies and headers.
pub(super) fn named_parameters(value: &Value) -> Option<Value> {
    let entries = value.get("parameters").and_then(Value::as_array)?;
    let mut mapped = Map::new();
    for entry in entries {
        let name = entry.get("name").and_then(Value::as_str)?;
        let value = entry.get("value").cloned().unwrap_or(Value::Null);
        mapped.insert(name.to_string(), value);
    }
    Some(Value::Object(mapped))
}
