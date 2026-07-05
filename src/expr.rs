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
//! # The evaluation scope
//!
//! For node config resolution the scope is built by `crate::nodes::expr_scope`
//! and exposes:
//!
//! - `item` — the first input item's `json` (the direct predecessors' output);
//! - `items` — every input item's `json`, in edge order;
//! - `run` — run metadata and the trigger payload;
//! - `nodes` — **every completed node's output, keyed by node id**:
//!   `{ "<id>": { "item": <first json>, "items": [<json>…] } }`. Use this to
//!   reference a node that is not the direct predecessor
//!   (`"=nodes.fetch_recipient.item.email"`) or to disambiguate the inputs of a
//!   fan-in node (`"=nodes.p.item.v"` vs `"=nodes.q.item.v"`). The jq form is
//!   `"=.nodes[\"fetch_recipient\"].items[0].email"`. Ids are the addressing
//!   key; node names are not indexed.
//!
//! [jq]: https://jqlang.org/
//! [`jaq`]: https://crates.io/crates/jaq

use jaq_core::load::{Arena, File, Loader};
use jaq_core::{Compiler, Ctx, Vars, data, unwrap_valr};
use jaq_json::Val;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A `=`-expression in a node's config that resolved to [`Value::Null`].
///
/// Emitted by [`resolve_traced`] so callers can surface *where* a binding came
/// up empty — a missing upstream field, a typo'd path, or a malformed jq
/// program all silently yield `null`, and without this record the failure only
/// shows up much later (e.g. deep inside an external tool call). Non-fatal:
/// resolution still produces the same config [`resolve`] would.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NullResolution {
    /// Dotted location of the config leaf that held the expression, with array
    /// elements as numeric segments — e.g. `args.to` or `args.cc.0`.
    pub location: String,
    /// The original expression string, including its `=` prefix.
    pub expression: String,
}

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
/// # Examples
///
/// ```
/// use serde_json::json;
/// use tinyflows::expr::evaluate;
///
/// let scope = json!({ "user": { "name": "Ada" }, "nums": [1, 2, 3] });
///
/// // A simple dotted path walks the scope segment by segment.
/// assert_eq!(evaluate(&json!("=user.name"), &scope), json!("Ada"));
///
/// // A leading dot routes to the jq engine; here, the array length.
/// assert_eq!(evaluate(&json!("=.nums | length"), &scope), json!(3));
///
/// // A missing path yields null rather than erroring.
/// assert_eq!(evaluate(&json!("=user.email"), &scope), json!(null));
///
/// // Non-expression values pass through as a literal clone.
/// assert_eq!(evaluate(&json!("literal"), &scope), json!("literal"));
/// assert_eq!(evaluate(&json!(42), &scope), json!(42));
/// ```
///
/// [jq]: https://jqlang.org/
#[must_use]
pub fn evaluate(value: &Value, scope: &Value) -> Value {
    match value.as_str() {
        Some(s) if is_expression(s) => {
            let expr = s[1..].trim();
            if is_simple_dotted_path(expr) {
                resolve_path(expr, scope)
            } else {
                run_jq(expr, scope)
            }
        }
        _ => value.clone(),
    }
}

/// Recursively resolves every `=`-expression embedded anywhere in a config
/// `value`, evaluating each against `scope`.
///
/// This is the data-binding entry point used by capability-backed integration
/// nodes: it walks a whole config tree and replaces each leaf that is a
/// `=`-expression string with the result of [`evaluate`], leaving everything
/// else untouched. The traversal is structural:
///
/// - an [`Object`](Value::Object) maps each of its values through `resolve`,
///   keeping keys as-is;
/// - an [`Array`](Value::Array) maps each element through `resolve`;
/// - a [`String`](Value::String) that [`is_expression`] is evaluated with
///   [`evaluate`] (a missing dotted path yields [`Value::Null`]);
/// - any other value (non-`=` string, number, bool, null) is returned as a
///   literal clone.
///
/// Resolution is therefore backward-compatible: config with no `=`-prefixed
/// strings round-trips unchanged.
///
/// # Examples
///
/// ```
/// use serde_json::json;
/// use tinyflows::expr::resolve;
///
/// let scope = json!({ "item": { "name": "Ada", "xs": [1, 2, 3] } });
/// let cfg = json!({
///     "slug": "slack.send",
///     "args": { "text": "=item.name", "count": "=.item.xs | length" },
///     "literal": 7,
/// });
/// assert_eq!(
///     resolve(&cfg, &scope),
///     json!({
///         "slug": "slack.send",
///         "args": { "text": "Ada", "count": 3 },
///         "literal": 7,
///     })
/// );
/// ```
#[must_use]
pub fn resolve(value: &Value, scope: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), resolve(v, scope)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(|v| resolve(v, scope)).collect()),
        Value::String(s) if is_expression(s) => evaluate(value, scope),
        _ => value.clone(),
    }
}

/// Like [`resolve`], but also reports every `=`-expression that resolved to
/// [`Value::Null`].
///
/// The resolved config is byte-identical to what [`resolve`] returns; the
/// second element lists a [`NullResolution`] per expression leaf whose value
/// came back `null` — whether from a missing path, a jq program with no
/// output, or a malformed program. Callers use this to warn ("arg `to`
/// resolved to null (expression `=item.to`)") **before** the null flows into
/// an external effect. An expression that legitimately produces `null` is
/// indistinguishable from a miss, which is why this traces rather than fails.
///
/// # Examples
///
/// ```
/// use serde_json::json;
/// use tinyflows::expr::resolve_traced;
///
/// let scope = json!({ "item": { "name": "Ada" } });
/// let cfg = json!({ "args": { "text": "=item.name", "to": "=item.email" } });
/// let (resolved, misses) = resolve_traced(&cfg, &scope);
/// assert_eq!(resolved, json!({ "args": { "text": "Ada", "to": null } }));
/// assert_eq!(misses.len(), 1);
/// assert_eq!(misses[0].location, "args.to");
/// assert_eq!(misses[0].expression, "=item.email");
/// ```
#[must_use]
pub fn resolve_traced(value: &Value, scope: &Value) -> (Value, Vec<NullResolution>) {
    let mut misses = Vec::new();
    let resolved = resolve_traced_inner(value, scope, &mut String::new(), &mut misses);
    (resolved, misses)
}

/// Recursive worker for [`resolve_traced`]: mirrors [`resolve`]'s traversal
/// while threading the dotted `location` of the current leaf and collecting
/// null-resolved expressions into `misses`.
fn resolve_traced_inner(
    value: &Value,
    scope: &Value,
    location: &mut String,
    misses: &mut Vec<NullResolution>,
) -> Value {
    /// Runs `f` with `segment` appended to `location`, restoring it after.
    fn with_segment<T>(
        location: &mut String,
        segment: &str,
        f: impl FnOnce(&mut String) -> T,
    ) -> T {
        let base = location.len();
        if !location.is_empty() {
            location.push('.');
        }
        location.push_str(segment);
        let out = f(location);
        location.truncate(base);
        out
    }

    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| {
                    let resolved = with_segment(location, k, |loc| {
                        resolve_traced_inner(v, scope, loc, misses)
                    });
                    (k.clone(), resolved)
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .enumerate()
                .map(|(i, v)| {
                    with_segment(location, &i.to_string(), |loc| {
                        resolve_traced_inner(v, scope, loc, misses)
                    })
                })
                .collect(),
        ),
        Value::String(s) if is_expression(s) => {
            let resolved = evaluate(value, scope);
            if resolved.is_null() {
                misses.push(NullResolution {
                    location: location.clone(),
                    expression: s.clone(),
                });
            }
            resolved
        }
        _ => value.clone(),
    }
}

/// Resolves a simple dotted path (e.g. `a.b`) by walking `scope` segment by
/// segment; a missing segment yields [`Value::Null`].
fn resolve_path(path: &str, scope: &Value) -> Value {
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

    // --- is_expression -----------------------------------------------------

    #[test]
    fn is_expression_detects_equals_prefix() {
        assert!(is_expression("=x"));
        assert!(is_expression("=")); // bare `=` is still flagged as an expression
        assert!(!is_expression("x"));
        assert!(!is_expression("")); // empty is not an expression
        assert!(!is_expression(" =x")); // leading space defeats the prefix
    }

    // --- literal passthrough ----------------------------------------------

    #[test]
    fn passes_through_non_equals_string() {
        let scope = json!({ "a": 1 });
        assert_eq!(evaluate(&json!("plain"), &scope), json!("plain"));
        // A string that merely contains `=` (not a prefix) is still a literal.
        assert_eq!(evaluate(&json!("a=b"), &scope), json!("a=b"));
    }

    #[test]
    fn passes_through_non_string_scalars() {
        let scope = json!({});
        assert_eq!(evaluate(&json!(42), &scope), json!(42));
        assert_eq!(evaluate(&json!(3.5), &scope), json!(3.5));
        assert_eq!(evaluate(&json!(true), &scope), json!(true));
        assert_eq!(evaluate(&json!(false), &scope), json!(false));
        assert_eq!(evaluate(&json!(null), &scope), json!(null));
    }

    #[test]
    fn passes_through_composite_literals() {
        let scope = json!({});
        assert_eq!(evaluate(&json!([1, 2, 3]), &scope), json!([1, 2, 3]));
        assert_eq!(evaluate(&json!({ "k": "v" }), &scope), json!({ "k": "v" }));
    }

    // --- dotted-path fast path --------------------------------------------

    #[test]
    fn dotted_path_resolves_nested() {
        let scope = json!({ "a": { "b": { "c": 7 } } });
        assert_eq!(evaluate(&json!("=a.b.c"), &scope), json!(7));
    }

    #[test]
    fn dotted_path_missing_segment_is_null() {
        let scope = json!({ "a": { "b": {} } });
        assert_eq!(evaluate(&json!("=a.b.c"), &scope), Value::Null);
        // A missing top-level segment is also Null.
        assert_eq!(evaluate(&json!("=x.y"), &scope), Value::Null);
    }

    #[test]
    fn dotted_path_through_non_object_is_null() {
        // Descending into a scalar (here a number) yields Null rather than
        // panicking: `Value::get` on a non-object returns `None`.
        let scope = json!({ "a": 5 });
        assert_eq!(evaluate(&json!("=a.b"), &scope), Value::Null);
        // Same for descending into an array with a name segment.
        let scope = json!({ "a": [1, 2, 3] });
        assert_eq!(evaluate(&json!("=a.b"), &scope), Value::Null);
    }

    #[test]
    fn dotted_path_single_segment() {
        let scope = json!({ "a": { "nested": true } });
        assert_eq!(evaluate(&json!("=a"), &scope), json!({ "nested": true }));
    }

    // --- jq programs -------------------------------------------------------

    #[test]
    fn jq_add_sums_array() {
        let scope = json!({ "item": { "nums": [1, 2, 3, 4] } });
        assert_eq!(evaluate(&json!("=.item.nums | add"), &scope), json!(10));
    }

    #[test]
    fn jq_length_of_string() {
        let scope = json!({ "item": { "name": "hello" } });
        assert_eq!(evaluate(&json!("=.item.name | length"), &scope), json!(5));
    }

    #[test]
    fn jq_map_doubles_each_element() {
        let scope = json!({ "item": { "nums": [1, 2, 3] } });
        assert_eq!(
            evaluate(&json!("=.item.nums | map(. * 2)"), &scope),
            json!([2, 4, 6])
        );
    }

    #[test]
    fn jq_select_keeps_matching_input() {
        // A passing predicate emits the input value.
        let scope = json!({ "item": { "n": 10 } });
        assert_eq!(
            evaluate(&json!("=.item.n | select(. > 5)"), &scope),
            json!(10)
        );
    }

    #[test]
    fn jq_select_filtering_out_yields_null() {
        // A failing predicate produces no output, which maps to Null.
        let scope = json!({ "item": { "n": 3 } });
        assert_eq!(
            evaluate(&json!("=.item.n | select(. > 5)"), &scope),
            Value::Null
        );
    }

    #[test]
    fn jq_arithmetic() {
        let scope = json!({ "item": { "a": 6, "b": 4 } });
        assert_eq!(evaluate(&json!("=.item.a + .item.b"), &scope), json!(10));
        assert_eq!(evaluate(&json!("=.item.a * .item.b"), &scope), json!(24));
    }

    #[test]
    fn jq_array_index() {
        let scope = json!({ "item": { "nums": [10, 20, 30] } });
        assert_eq!(evaluate(&json!("=.item.nums[0]"), &scope), json!(10));
        assert_eq!(evaluate(&json!("=.item.nums[2]"), &scope), json!(30));
    }

    #[test]
    fn jq_object_construction() {
        let scope = json!({ "item": { "first": "Ada", "last": "Lovelace" } });
        assert_eq!(
            evaluate(&json!("={name: .item.first, surname: .item.last}"), &scope),
            json!({ "name": "Ada", "surname": "Lovelace" })
        );
    }

    #[test]
    fn jq_string_operations() {
        let scope = json!({ "item": { "first": "Ada", "last": "Lovelace" } });
        // String concatenation.
        assert_eq!(
            evaluate(&json!(r#"=.item.first + " " + .item.last"#), &scope),
            json!("Ada Lovelace")
        );
        // A standard-library string builtin.
        assert_eq!(
            evaluate(&json!("=.item.first | ascii_upcase"), &scope),
            json!("ADA")
        );
    }

    #[test]
    fn jq_first_output_only() {
        // A program that yields multiple outputs returns only the first.
        let scope = json!({});
        assert_eq!(evaluate(&json!("=1, 2, 3"), &scope), json!(1));
    }

    #[test]
    fn item_shorthand_versus_leading_dot() {
        // `=item.x` takes the segment-walk fast path; `=.item.x` takes jq.
        // Both must resolve to the same value for a plain object scope.
        let scope = json!({ "item": { "x": 99 } });
        assert_eq!(evaluate(&json!("=item.x"), &scope), json!(99));
        assert_eq!(evaluate(&json!("=.item.x"), &scope), json!(99));
    }

    #[test]
    fn jq_malformed_program_is_null() {
        let scope = json!({ "item": {} });
        assert_eq!(evaluate(&json!("=.item |"), &scope), Value::Null);
        assert_eq!(evaluate(&json!("=(((("), &scope), Value::Null);
    }

    // --- resolve (recursive config data-binding) --------------------------

    #[test]
    fn resolve_maps_nested_objects_and_arrays() {
        let scope = json!({ "item": { "name": "Ada", "id": 7 } });
        let cfg = json!({
            "slug": "x.y",
            "args": { "text": "=item.name", "list": ["=item.id", "static"] },
        });
        assert_eq!(
            resolve(&cfg, &scope),
            json!({
                "slug": "x.y",
                "args": { "text": "Ada", "list": [7, "static"] },
            })
        );
    }

    #[test]
    fn resolve_passes_through_non_expression_leaves() {
        let scope = json!({ "item": { "name": "Ada" } });
        // Non-`=` strings, numbers, bools, and null all pass through unchanged.
        assert_eq!(resolve(&json!("plain"), &scope), json!("plain"));
        assert_eq!(resolve(&json!("a=b"), &scope), json!("a=b"));
        assert_eq!(resolve(&json!(42), &scope), json!(42));
        assert_eq!(resolve(&json!(3.5), &scope), json!(3.5));
        assert_eq!(resolve(&json!(true), &scope), json!(true));
        assert_eq!(resolve(&json!(null), &scope), json!(null));
    }

    #[test]
    fn resolve_missing_dotted_path_is_null() {
        let scope = json!({ "item": { "name": "Ada" } });
        assert_eq!(
            resolve(&json!({ "who": "=item.email" }), &scope),
            json!({ "who": null })
        );
    }

    #[test]
    fn resolve_evaluates_jaq_program_in_nested_field() {
        let scope = json!({ "item": { "xs": [1, 2, 3, 4] } });
        assert_eq!(
            resolve(&json!({ "n": "=.item.xs | length" }), &scope),
            json!({ "n": 4 })
        );
    }

    #[test]
    fn resolve_leaves_config_without_expressions_unchanged() {
        let scope = json!({ "item": { "name": "Ada" } });
        let cfg = json!({ "a": 1, "b": ["x", 2, true], "c": { "d": "plain" } });
        assert_eq!(resolve(&cfg, &scope), cfg);
    }

    // --- resolve_traced (null-resolution diagnostics) ----------------------

    #[test]
    fn resolve_traced_matches_resolve_and_reports_misses() {
        let scope = json!({ "item": { "name": "Ada" } });
        let cfg = json!({
            "slug": "gmail.send",
            "args": { "text": "=item.name", "to": "=item.email", "cc": ["=item.cc"] },
            "literal": null,
        });
        let (resolved, misses) = resolve_traced(&cfg, &scope);
        // The resolved config is identical to `resolve`'s.
        assert_eq!(resolved, resolve(&cfg, &scope));
        // Only the `=`-expressions that came back null are reported — the
        // literal null and the successful binding are not.
        let mut locations: Vec<&str> = misses.iter().map(|m| m.location.as_str()).collect();
        locations.sort_unstable();
        assert_eq!(locations, vec!["args.cc.0", "args.to"]);
        let to = misses
            .iter()
            .find(|m| m.location == "args.to")
            .expect("to miss");
        assert_eq!(to.expression, "=item.email");
    }

    #[test]
    fn resolve_traced_reports_malformed_jq_program() {
        let scope = json!({});
        let (resolved, misses) = resolve_traced(&json!({ "n": "=((bad jq" }), &scope);
        assert_eq!(resolved, json!({ "n": null }));
        assert_eq!(misses.len(), 1);
        assert_eq!(misses[0].location, "n");
    }

    #[test]
    fn resolve_traced_clean_config_has_no_misses() {
        let scope = json!({ "item": { "x": 1 } });
        let cfg = json!({ "a": "=item.x", "b": "plain", "c": 7 });
        let (resolved, misses) = resolve_traced(&cfg, &scope);
        assert_eq!(resolved, json!({ "a": 1, "b": "plain", "c": 7 }));
        assert!(misses.is_empty());
    }

    // --- property-based tests ---------------------------------------------
    //
    // These assert the "never panic on arbitrary input" contract: no matter
    // what program string or scope is thrown at `evaluate`, it must return
    // *some* `Value` (never unwind). A bounded, shallow JSON strategy keeps
    // the jq engine's work small so the whole suite stays well under a second.

    use proptest::prelude::*;

    /// A bounded, recursive `serde_json::Value` strategy. Leaves are simple
    /// scalars; arrays/objects nest at most a few levels deep with a handful of
    /// elements, keeping generated scopes small enough for fast jq evaluation.
    fn arb_json() -> impl Strategy<Value = Value> {
        let leaf = prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::from),
            any::<i32>().prop_map(Value::from),
            // Restrict strings to short identifier-ish text so generated object
            // keys are realistic and the search space stays small.
            "[A-Za-z0-9_]{0,8}".prop_map(Value::from),
        ];
        leaf.prop_recursive(3, 16, 4, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..4).prop_map(Value::from),
                prop::collection::hash_map("[A-Za-z_][A-Za-z0-9_]{0,5}", inner, 0..4)
                    .prop_map(|m| Value::from(m.into_iter().collect::<serde_json::Map<_, _>>())),
            ]
        })
    }

    proptest! {
        /// `evaluate` never panics for an arbitrary `=`-prefixed program run
        /// against an arbitrary bounded JSON scope — it always yields a `Value`.
        #[test]
        fn prop_evaluate_never_panics_on_expression(program in ".*", scope in arb_json()) {
            let value = Value::from(format!("={program}"));
            // The mere fact this returns (rather than unwinding) is the property;
            // consume the result so it is not optimized away.
            let out = evaluate(&value, &scope);
            let _ = out;
        }

        /// A non-`=` string is returned verbatim as a `Value::String` literal.
        #[test]
        fn prop_non_expression_string_is_literal(s in ".*", scope in arb_json()) {
            prop_assume!(!s.starts_with('='));
            prop_assert_eq!(evaluate(&Value::from(s.clone()), &scope), Value::String(s));
        }

        /// `is_expression` is exactly the `=`-prefix test for any string.
        #[test]
        fn prop_is_expression_matches_prefix(s in ".*") {
            prop_assert_eq!(is_expression(&s), s.starts_with('='));
        }

        /// `=` + an arbitrary simple dotted path never panics and resolves to
        /// either `Null` (missing/non-object segment) or a subtree of the scope.
        #[test]
        fn prop_dotted_path_resolves_or_null(
            segments in prop::collection::vec("[A-Za-z_][A-Za-z0-9_]{0,5}", 1..4),
            scope in arb_json(),
        ) {
            let path = segments.join(".");
            let out = evaluate(&Value::from(format!("={path}")), &scope);
            // Walk the same path by hand; the fast path must agree with it.
            let mut expected = &scope;
            let mut resolved = true;
            for seg in &segments {
                match expected.get(seg) {
                    Some(next) => expected = next,
                    None => {
                        resolved = false;
                        break;
                    }
                }
            }
            if resolved {
                prop_assert_eq!(out, expected.clone());
            } else {
                prop_assert_eq!(out, Value::Null);
            }
        }
    }
}
