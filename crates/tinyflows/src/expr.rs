//! Expression evaluation for node config values (the `=`-prefix convention).
//!
//! A config string beginning with `=` is an **expression**. The remainder is a
//! [jq] program, compiled and executed by [`jaq`], with the entire evaluation
//! *scope* as the jq input (`.`). For example, `"=.item.name"` yields
//! `scope["item"]["name"]`, and `"=.item.items | length"` yields the length of
//! that array. Only the **first** output value of the program is returned.
//!
//! As a backward-compatible shorthand, a remainder that is a *simple dotted
//! path* — segments matching `[A-Za-z_][A-Za-z0-9_-]*` joined by `.`, e.g.
//! `"=item.name"` or `"=nodes.my-node.item.x"` — is resolved by a direct
//! segment-walk over the scope ([`Value::Null`] for a missing segment) instead
//! of the jq engine, so legacy expressions keep their exact behavior and
//! hyphenated node ids stay addressable (the hyphen is a literal key char, not
//! jq subtraction).
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

use std::borrow::Cow;

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
/// `^[A-Za-z_][A-Za-z0-9_-]*(\.[A-Za-z_][A-Za-z0-9_-]*)*$`.
fn is_simple_dotted_path(s: &str) -> bool {
    !s.is_empty() && s.split('.').all(is_ident)
}

/// Returns true if `seg` is a non-empty path segment: an ASCII letter or `_`
/// followed by ASCII alphanumerics, `_`, or `-`. The hyphen is permitted so
/// that hyphenated node ids (e.g. `nodes.my-node.item.x`) resolve via the
/// literal segment-walk instead of falling through to jq, which would parse
/// `my-node` as subtraction. A real jq program still routes to jq: its spaces,
/// pipes, operators, or leading dot make at least one segment fail this check
/// (and a leading-digit segment like `1 - 2` is rejected outright).
fn is_ident(seg: &str) -> bool {
    let mut chars = seg.chars();
    match chars.next() {
        Some(c) if c == '_' || c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c == '_' || c == '-' || c.is_ascii_alphanumeric())
}

/// Repairs a hybrid expression that mixes the simple-path shorthand's bare
/// scope key (e.g. `item`, with no leading dot) with a real jq pipe, such as
/// `"item.labels | any(.name == \"in progress\") | not"`. That whole string
/// fails [`is_simple_dotted_path`] (it contains a `|`), so [`evaluate`] routes
/// it to [`run_jq`] as a jq program — but a *bare* `item` at the start of a jq
/// program parses as a call to an undefined jq function named `item`, so the
/// program fails to compile and [`run_jq`] silently returns [`Value::Null`]
/// (which, fed into a `condition` node's truthiness check, routes every item
/// to `false`).
///
/// If `program` starts with a bare identifier
/// (`[A-Za-z_][A-Za-z0-9_-]*`) that is a **top-level key of `scope`** (e.g.
/// `item`/`items`/`run`/`nodes`, per `nodes::expr_scope_for`), and the
/// character immediately following that identifier is `.`, `|`, `[`, ` `, or
/// end-of-string, this prepends a `.` so the identifier is read as a jq field
/// access on the scope root (`.item...`) instead of a function call. A
/// following `(` is deliberately excluded from that boundary set: it marks a
/// jq function *call* (`any(...)`, `map(...)`) rather than a field reference,
/// so `any(...)`/`map(...)` are left untouched even if `scope` happened to
/// have a same-named top-level key.
///
/// Programs that do not start with a scope-key identifier (leading-dot jq
/// like `.item...`, or an identifier scope doesn't recognize) are returned
/// unchanged — this only changes expressions that would otherwise
/// unconditionally fail to compile and resolve to `Null`. Plain dotted paths
/// with no jq (`"item.labels"`, no `|`/`[`/space) never reach `run_jq` at all:
/// [`evaluate`] takes the [`is_simple_dotted_path`] fast path for those.
fn normalize_bare_scope_ref<'a>(program: &'a str, scope: &Value) -> Cow<'a, str> {
    let mut chars = program.char_indices();
    let Some((_, first)) = chars.next() else {
        return Cow::Borrowed(program);
    };
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return Cow::Borrowed(program);
    }

    // Walk the rest of the leading identifier (ASCII letters/digits/`_`/`-`),
    // tracking the byte offset just past its last matching char.
    let mut end = first.len_utf8();
    for (i, c) in chars {
        if c == '_' || c == '-' || c.is_ascii_alphanumeric() {
            end = i + c.len_utf8();
        } else {
            break;
        }
    }

    let ident = &program[..end];
    if scope.get(ident).is_none() {
        return Cow::Borrowed(program);
    }

    let boundary_ok = match program[end..].chars().next() {
        None => true,
        Some(c) => matches!(c, '.' | '|' | '[' | ' '),
    };
    if boundary_ok {
        Cow::Owned(format!(".{program}"))
    } else {
        Cow::Borrowed(program)
    }
}

/// Compiles `program` as a jq filter and runs it once against `scope` (the jq
/// input `.`), returning the first output value. Any compile/run failure,
/// non-JSON output, or empty output yields [`Value::Null`]; this never panics.
fn run_jq(program: &str, scope: &Value) -> Value {
    let program = normalize_bare_scope_ref(program, scope);
    let program = program.as_ref();

    // Convert the scope into a jq value; bail to Null if it cannot be represented.
    let Ok(input) = serde_json::from_value::<Val>(scope.clone()) else {
        return Value::Null;
    };

    // `jaq-std` keeps its jq-authored definitions in one file, including
    // wrappers around optional native functions. Keep the definitions that
    // compile against `base_funs`; omit wrappers for the disabled bundles.
    let defs = jaq_core::defs()
        .chain(jaq_std::defs().filter(|def| {
            !(matches!(
                def.name,
                "stderr"
                    | "debug"
                    | "halt_error"
                    | "logb"
                    | "significand"
                    | "pow10"
                    | "drem"
                    | "nexttoward"
                    | "scalb"
                    | "gamma"
                    | "capture_of_match"
                    | "test"
                    | "scan"
                    | "match"
                    | "capture"
                    | "splits"
                    | "sub"
                    | "gsub"
                    | "todate"
                    | "fromdate"
                    | "@html"
                    | "@htmld"
                    | "@uri"
                    | "@urid"
                    | "@base64"
                    | "@base64d"
            ) || (def.name == "split" && def.args.len() == 2))
        }))
        .chain(jaq_json::defs());
    // Keep the minimal, value-generic standard library. Supplementary native
    // builtins are deliberately not compiled: besides pulling in time, regex,
    // formatting, and specialist-math stacks, that set includes process-facing
    // `env`/`$ENV`, which could leak host secrets into workflow output.
    let funs = jaq_core::funs()
        .chain(jaq_std::base_funs())
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
#[path = "expr_tests.rs"]
mod tests;
