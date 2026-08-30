//! Translating n8n's `={{ … }}` expressions into jq.
//!
//! The bar is not "produces something": a translation that parses but reads the
//! wrong field resolves to null at run time, which is the failure the importer's
//! warnings exist to keep visible.

use super::*;

#[test]
fn translates_trivial_json_expression_to_jq() {
    // R-C1: n8n's `$json` is the CURRENT INPUT ITEM, which the tinyflows
    // scope binds under `item` (scope root is `{item, items, run,
    // nodes}` — `vendor/tinyflows/src/nodes/mod.rs::expr_scope_for`).
    // The translated jq path must therefore dereference `.item` first —
    // `=.email` (the old, buggy output) dereferences a key that doesn't
    // exist at the scope root and is GUARANTEED to resolve `null`.
    let mut warnings = Vec::new();
    assert_eq!(
        translate_expr("={{ $json.email }}", &mut warnings, "n"),
        "=.item.email"
    );
    assert_eq!(
        translate_expr("={{ $json.user.name }}", &mut warnings, "n"),
        "=.item.user.name"
    );
    // A bracket key with a space isn't a bare jq identifier — must come out
    // quoted (`."first name"`), not `.first name` (invalid jq).
    assert_eq!(
        translate_expr("={{ $json[\"first name\"] }}", &mut warnings, "n"),
        "=.item.\"first name\""
    );
    // `{{ $json }}` (no tail) must become `.item`, NOT the bare `.` root —
    // `.` would return the whole `{item, items, run, nodes}` scope object,
    // not the item.
    assert_eq!(translate_expr("={{ $json }}", &mut warnings, "n"), "=.item");
    assert!(warnings.is_empty());
}

/// R-C1 regression proof: actually evaluate the translated expression
/// against a scope shaped exactly like the tinyflows engine's real
/// `expr_scope_for` (`vendor/tinyflows/src/nodes/mod.rs`), and assert it
/// resolves the real field — NOT `null`. This is the check that would
/// have caught the original bug: the old `=.email` translation compiles
/// fine and passes structural validation, it just silently resolves
/// `null` at runtime because `email` isn't a key at the scope root.
#[test]
fn translated_json_expression_resolves_against_the_real_engine_scope() {
    let mut warnings = Vec::new();
    let translated = translate_expr("={{ $json.email }}", &mut warnings, "n");
    assert!(warnings.is_empty());

    let scope = json!({
        "item": { "email": "person@example.com" },
        "items": [{ "email": "person@example.com" }],
        "run": {},
        "nodes": {},
    });
    assert_eq!(
        tinyflows::expr::evaluate(&json!(translated), &scope),
        json!("person@example.com"),
        "translated expression `{translated}` must resolve the real field, not null"
    );

    // The pre-fix translation (`=.email`) is left here as a negative
    // control: it dereferences a key that does not exist at the scope
    // root and is GUARANTEED to resolve null — exactly the bug R-C1
    // describes.
    assert_eq!(
        tinyflows::expr::evaluate(&json!("=.email"), &scope),
        Value::Null
    );

    // Bare `{{ $json }}` → `.item` must resolve the WHOLE item, not the
    // whole scope (which would additionally carry `items`/`run`/`nodes`).
    let translated_whole = translate_expr("={{ $json }}", &mut warnings, "n");
    assert_eq!(
        tinyflows::expr::evaluate(&json!(translated_whole), &scope),
        json!({ "email": "person@example.com" })
    );
}

#[test]
fn jq_field_quotes_non_bare_identifiers() {
    // Plain identifiers stay bare.
    assert_eq!(jq_field("foo"), "foo");
    assert_eq!(jq_field("foo_bar"), "foo_bar");
    // Spaces, punctuation, and digit-leading keys aren't bare jq
    // identifiers — jq requires the dot-plus-quoted-string form for these.
    assert_eq!(jq_field("first name"), "\"first name\"");
    assert_eq!(jq_field("foo-bar"), "\"foo-bar\"");
    assert_eq!(jq_field("123key"), "\"123key\"");
}

#[test]
fn leaves_untranslatable_expression_raw_with_warning() {
    let mut warnings = Vec::new();
    let raw = "={{ $json.a + $json.b }}";
    assert_eq!(translate_expr(raw, &mut warnings, "Math"), raw);
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("not automatically translated"));
}

#[test]
fn plain_string_is_not_treated_as_expression() {
    let mut warnings = Vec::new();
    assert_eq!(translate_expr("hello", &mut warnings, "n"), "hello");
    assert!(warnings.is_empty());
}
