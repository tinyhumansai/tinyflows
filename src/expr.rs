//! Minimal expression evaluation for node config values (the `=`-prefix convention).
//!
//! A config string beginning with `=` is an **expression**: the remainder is a
//! dotted path resolved against a scope (e.g. `"=item.name"` -> `scope["item"]["name"]`).
//! Anything else is a literal. Full jq-style expressions are a future upgrade.

use serde_json::Value;

/// Returns true if `s` is an expression (begins with `=`).
#[must_use]
pub fn is_expression(s: &str) -> bool {
    s.starts_with('=')
}

/// Evaluates a config `value` against `scope`. A string like `"=a.b"` resolves the
/// dotted path `a.b` within `scope` (missing segments yield [`Value::Null`]); any
/// other value is returned as a literal clone.
#[must_use]
pub fn evaluate(value: &Value, scope: &Value) -> Value {
    match value.as_str() {
        Some(s) if is_expression(s) => resolve(s[1..].trim(), scope),
        _ => value.clone(),
    }
}

fn resolve(path: &str, scope: &Value) -> Value {
    let mut current = scope;
    for segment in path.split('.').filter(|s| !s.is_empty()) {
        match current.get(segment) {
            Some(next) => current = next,
            None => return Value::Null,
        }
    }
    current.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn passes_through_literals() {
        let scope = json!({});
        assert_eq!(evaluate(&json!("hello"), &scope), json!("hello"));
        assert_eq!(evaluate(&json!(42), &scope), json!(42));
    }

    #[test]
    fn resolves_a_reference() {
        let scope = json!({ "user": { "email": "a@b.com" } });
        assert_eq!(evaluate(&json!("=user.email"), &scope), json!("a@b.com"));
    }

    #[test]
    fn missing_path_is_null() {
        let scope = json!({ "user": { "email": "a@b.com" } });
        assert_eq!(evaluate(&json!("=user.name"), &scope), Value::Null);
    }
}
