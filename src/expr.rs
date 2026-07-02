//! Expression evaluation for node config values (the `=`-prefix convention).
//!
//! A config string beginning with `=` is an **expression**. The remainder is a
//! [jq] program, compiled and executed by [`jaq`], with the entire evaluation
//! *scope* as the jq input (`.`). For example, `"=.item.name"` yields
//! `scope["item"]["name"]`, and `"=.item.items | length"` yields the length of
//! that array. Only the **first** output value of the program is returned.
//!
//! As a backward-compatible shorthand, a remainder that is a *simple dotted
//! path* — segments matching `[A-Za-z_][A-Za-z0-9_]*` joined by `.`, e.g.
//! `"=item.name"` — is resolved by a direct segment-walk over the scope
//! ([`Value::Null`] for a missing segment) instead of the jq engine, so legacy
//! expressions keep their exact behavior.
//!
//! Anything that is not a `=`-prefixed string is returned as a literal clone.
//! jq programs never panic: a compile/run error, non-JSON output, or empty
//! output all yield [`Value::Null`].
//!
//! [jq]: https://jqlang.org/
//! [`jaq`]: https://crates.io/crates/jaq

use jaq_core::load::{Arena, File, Loader};
use jaq_core::{Compiler, Ctx, Vars, data, unwrap_valr};
use jaq_json::Val;
use serde_json::Value;

/// Returns true if `s` is an expression (begins with `=`).
#[must_use]
pub fn is_expression(s: &str) -> bool {
    s.starts_with('=')
}

/// Evaluates a config `value` against `scope`.
///
/// A string like `"=item.name"` (a simple dotted path) resolves that path
/// within `scope`, with missing segments yielding [`Value::Null`]. Any other
/// `=`-prefixed string is treated as a [jq] program run against `scope` (see
/// the [module docs](self)), returning its first output or [`Value::Null`].
/// Non-expression values are returned as a literal clone.
///
/// [jq]: https://jqlang.org/
#[must_use]
pub fn evaluate(value: &Value, scope: &Value) -> Value {
    match value.as_str() {
        Some(s) if is_expression(s) => {
            let expr = s[1..].trim();
            if is_simple_dotted_path(expr) {
                resolve(expr, scope)
            } else {
                run_jq(expr, scope)
            }
        }
        _ => value.clone(),
    }
}

/// Resolves a simple dotted path (e.g. `a.b`) by walking `scope` segment by
/// segment; a missing segment yields [`Value::Null`].
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

/// Returns true if `s` is a simple dotted path:
/// `^[A-Za-z_][A-Za-z0-9_]*(\.[A-Za-z_][A-Za-z0-9_]*)*$`.
fn is_simple_dotted_path(s: &str) -> bool {
    !s.is_empty() && s.split('.').all(is_ident)
}

/// Returns true if `seg` is a non-empty identifier: an ASCII letter or `_`
/// followed by ASCII alphanumerics or `_`.
fn is_ident(seg: &str) -> bool {
    let mut chars = seg.chars();
    match chars.next() {
        Some(c) if c == '_' || c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// Compiles `program` as a jq filter and runs it once against `scope` (the jq
/// input `.`), returning the first output value. Any compile/run failure,
/// non-JSON output, or empty output yields [`Value::Null`]; this never panics.
fn run_jq(program: &str, scope: &Value) -> Value {
    // Convert the scope into a jq value; bail to Null if it cannot be represented.
    let Ok(input) = serde_json::from_value::<Val>(scope.clone()) else {
        return Value::Null;
    };

    let defs = jaq_core::defs()
        .chain(jaq_std::defs())
        .chain(jaq_json::defs());
    let funs = jaq_core::funs()
        .chain(jaq_std::funs())
        .chain(jaq_json::funs());

    let loader = Loader::new(defs);
    let arena = Arena::default();

    let file = File {
        code: program,
        path: (),
    };
    let Ok(modules) = loader.load(&arena, file) else {
        return Value::Null;
    };

    let Ok(filter) = Compiler::default().with_funs(funs).compile(modules) else {
        return Value::Null;
    };

    let ctx = Ctx::<data::JustLut<Val>>::new(&filter.lut, Vars::new([]));
    let mut out = filter.id.run((ctx, input)).map(unwrap_valr);

    match out.next() {
        // `Val`'s `Display` emits compact JSON, so re-parsing round-trips it
        // back into a `serde_json::Value`.
        Some(Ok(val)) => serde_json::from_str(&val.to_string()).unwrap_or(Value::Null),
        _ => Value::Null,
    }
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

    #[test]
    fn simple_dotted_path_uses_fast_path() {
        // No leading dot: matches the simple-path grammar and must resolve via
        // the segment-walk, exactly as before jq was introduced.
        assert!(is_simple_dotted_path("item.name"));
        assert!(!is_simple_dotted_path(".item.name"));
        let scope = json!({ "item": { "name": "Ada" } });
        assert_eq!(evaluate(&json!("=item.name"), &scope), json!("Ada"));
    }

    #[test]
    fn jq_leading_dot_path() {
        let scope = json!({ "item": { "name": "Ada" } });
        assert_eq!(evaluate(&json!("=.item.name"), &scope), json!("Ada"));
    }

    #[test]
    fn jq_pipe_and_length() {
        let scope = json!({ "item": { "items": [1, 2, 3] } });
        assert_eq!(evaluate(&json!("=.item.items | length"), &scope), json!(3));
    }

    #[test]
    fn jq_array_construction() {
        let scope = json!({ "item": { "a": 1, "b": 2 } });
        assert_eq!(
            evaluate(&json!("=[.item.a, .item.b]"), &scope),
            json!([1, 2])
        );
    }

    #[test]
    fn jq_bad_program_is_null() {
        let scope = json!({ "item": {} });
        assert_eq!(
            evaluate(&json!("=this is not ( valid jq"), &scope),
            Value::Null
        );
    }

    #[test]
    fn jq_empty_output_is_null() {
        // `empty` produces no outputs.
        let scope = json!({});
        assert_eq!(evaluate(&json!("=empty"), &scope), Value::Null);
    }
}
