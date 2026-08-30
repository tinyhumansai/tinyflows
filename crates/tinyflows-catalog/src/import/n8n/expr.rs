//! n8n expression (`={{ ... }}`) translation into tinyflows `=`-jq syntax.

use serde_json::{Map, Value};

/// Recursively translates n8n `={{ … }}` expressions inside a config `Value`
/// into tinyflows' `=`-prefixed jq form where trivially possible; anything not
/// trivially translatable is left as its raw string and a warning is recorded.
pub(super) fn translate_config(value: &Value, warnings: &mut Vec<String>, n8n_name: &str) -> Value {
    match value {
        Value::String(s) => Value::String(translate_expr(s, warnings, n8n_name)),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|v| translate_config(v, warnings, n8n_name))
                .collect(),
        ),
        Value::Object(map) => {
            let mut out = Map::new();
            for (k, v) in map {
                out.insert(k.clone(), translate_config(v, warnings, n8n_name));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

/// Translates a single n8n expression string. An n8n expression is a string
/// beginning with `=` whose body is `{{ … }}`. The trivially-translatable case
/// is a single `{{ $json.<path> }}` reference, which becomes tinyflows'
/// `=.item.<path>` jq. Anything richer is returned unchanged with a warning.
///
/// n8n's `$json` is the current input item's json — the tinyflows expression
/// scope root is `{item, items, run, nodes}` (`vendor/tinyflows/src/nodes/mod.rs::expr_scope_for`),
/// which binds exactly that under `item`. So `$json` must translate to
/// `.item`, never the bare `.` root: `.` would resolve the WHOLE scope object
/// (`item` + `items` + `run` + `nodes`), not the item, and `.<path>` without
/// the `item` segment dereferences a key that does not exist at the scope
/// root and is GUARANTEED to resolve `null` at runtime (R-C1).
pub(super) fn translate_expr(raw: &str, warnings: &mut Vec<String>, n8n_name: &str) -> String {
    // Only n8n expression strings start with `=`; plain values pass through.
    if !raw.starts_with('=') {
        return raw.to_string();
    }
    let body = raw[1..].trim();
    let Some(inner) = body
        .strip_prefix("{{")
        .and_then(|b| b.strip_suffix("}}"))
        .map(str::trim)
    else {
        // A `=`-prefixed non-`{{ }}` string: keep raw, warn.
        warnings.push(untranslated_warning(n8n_name, raw));
        return raw.to_string();
    };

    // Trivial single-reference case: `$json.foo.bar` (or `$json["foo"]`).
    if let Some(path) = inner.strip_prefix("$json") {
        if let Some(jq_path) = json_path_to_jq(path) {
            // `json_path_to_jq` returns the tail path RELATIVE to `$json`
            // (`.foo`, `.foo.bar`, or `""` when there's no tail at all — see
            // its doc). Prefixing `.item` here binds it at the tinyflows
            // scope's `item` key instead of the scope root (see this fn's
            // doc, R-C1): the empty-tail case naturally becomes `.item` (not
            // `.item.`) because the tail is `""`, with no special-casing
            // needed here.
            tracing::debug!(
                target: "flows",
                node = %n8n_name,
                %raw,
                translated = %format!("=.item{jq_path}"),
                "[flows] n8n_import: translated $json expression"
            );
            return format!("=.item{jq_path}");
        }
    }

    warnings.push(untranslated_warning(n8n_name, raw));
    raw.to_string()
}

/// Turns an n8n `$json` accessor tail (`.foo.bar`, `["foo"]`, or empty) into a
/// jq path relative to `$json` itself (`.foo.bar`, `.foo`, or `""` for an
/// empty tail — deliberately NOT `.`, so the caller can prepend `.item`
/// without special-casing the empty case, see [`translate_expr`]). Returns
/// `None` for anything that isn't a plain dotted / bracketed-string path
/// (arithmetic, function calls, bracket-index into arrays, etc.), so the
/// caller falls back to raw + warn.
pub(super) fn json_path_to_jq(tail: &str) -> Option<String> {
    let tail = tail.trim();
    if tail.is_empty() {
        return Some(String::new());
    }
    let mut jq = String::new();
    let mut rest = tail;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix('.') {
            // `.identifier`
            let end = after
                .find(|c: char| !c.is_alphanumeric() && c != '_')
                .unwrap_or(after.len());
            if end == 0 {
                return None;
            }
            jq.push('.');
            jq.push_str(&after[..end]);
            rest = &after[end..];
        } else {
            // `["identifier"]` or `['identifier']` — string keys only.
            let after = rest.strip_prefix('[')?;
            let close = after.find(']')?;
            let key = after[..close].trim();
            let key = key
                .strip_prefix('"')
                .and_then(|k| k.strip_suffix('"'))
                .or_else(|| key.strip_prefix('\'').and_then(|k| k.strip_suffix('\'')))?;
            if key.is_empty() {
                return None;
            }
            jq.push('.');
            jq.push_str(&jq_field(key));
            rest = &after[close + 1..];
        }
    }
    Some(jq)
}

/// Renders a single jq field-access key: bare (`foo`) when `key` is a plain
/// identifier (alphanumeric/underscore, not digit-leading), else quoted
/// (`"first name"`) per jq's dot-plus-quoted-string syntax — required for any
/// key containing spaces or punctuation, which `.foo bar` (unquoted) is not
/// valid jq for.
pub(super) fn jq_field(key: &str) -> String {
    let is_bare_identifier = !key.is_empty()
        && !key.starts_with(|c: char| c.is_ascii_digit())
        && key.chars().all(|c| c.is_alphanumeric() || c == '_');
    if is_bare_identifier {
        key.to_string()
    } else {
        format!("{:?}", key)
    }
}

pub(super) fn untranslated_warning(n8n_name: &str, raw: &str) -> String {
    format!(
        "Node '{n8n_name}' uses an n8n expression that was not automatically translated \
         (`{raw}`) — it was kept as a raw string. Review and rewrite it as a tinyflows \
         `=`-jq expression."
    )
}
