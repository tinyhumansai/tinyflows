

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
fn env_builtin_is_not_reachable() {
    // Security: the jaq-std `env` builtin dumps the host process environment
    // (API keys, tokens). It must be stripped, so an expression calling it
    // yields Null instead of leaking secrets. The test process always has a
    // populated environment (PATH etc.), so a reachable `env` would return a
    // non-null object; asserting Null proves it was removed. `$ENV` (the jq
    // env variable, if present) must be equally unreachable.
    let scope = json!({});
    assert_eq!(evaluate(&json!("=env"), &scope), Value::Null);
    assert_eq!(evaluate(&json!("=env.PATH"), &scope), Value::Null);
    assert_eq!(evaluate(&json!("=$ENV.PATH"), &scope), Value::Null);
    // A pure jq builtin still works; only supplementary native builtins
    // are omitted.
    assert_eq!(evaluate(&json!("=[1,2,3]|length"), &scope), json!(3));
}

#[test]
fn optional_native_builtins_are_not_reachable() {
    let scope = json!({});
    assert_eq!(evaluate(&json!("=now | ."), &scope), Value::Null);
    assert_eq!(
        evaluate(&json!("=\"<b>\" | escape_html"), &scope),
        Value::Null
    );
    assert_eq!(
        evaluate(&json!("=\"abc\" | matches(\"a\"; \"\")"), &scope),
        Value::Null
    );
    assert_eq!(
        evaluate(
            &json!("=\"2024-01-01T00:00:00Z\" | fromdateiso8601"),
            &scope
        ),
        Value::Null
    );
    assert_eq!(evaluate(&json!("=9 | sqrt"), &scope), Value::Null);
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

// --- bare-scope-key normalization (hybrid shorthand + jq pipe) ---------

#[test]
fn bare_scope_key_with_jq_pipe_is_normalized() {
    // `item.labels | any(...) | not` is a hybrid: no leading dot (like the
    // simple-path shorthand), but it has a jq pipe, so it fails
    // `is_simple_dotted_path` and falls to `run_jq`, where a bare `item`
    // would otherwise parse as an undefined jq function and fail to
    // compile. It must be normalized to `.item...` and evaluate for real.
    let program = r#"=item.labels | any(.name == "in progress") | not"#;

    let with_label = json!({
        "item": { "labels": [{ "name": "in progress" }] },
    });
    assert_eq!(
        evaluate(&json!(program), &with_label),
        json!(false),
        "an item carrying the 'in progress' label must route/evaluate to false"
    );

    let without_label = json!({
        "item": { "labels": [{ "name": "done" }] },
    });
    assert_eq!(
        evaluate(&json!(program), &without_label),
        json!(true),
        "an item without the 'in progress' label must route/evaluate to true"
    );
}

#[test]
fn bare_scope_key_normalization_does_not_break_leading_dot() {
    // An expression that already starts with `.` is untouched by
    // normalization (its first char is `.`, not an identifier char), so
    // no double-dot (`..item`) is ever introduced.
    let scope = json!({ "item": { "labels": [{ "name": "urgent" }] } });
    assert_eq!(
        evaluate(&json!(r#"=.item.labels | any(.name == "urgent")"#), &scope),
        json!(true)
    );
}

#[test]
fn bare_scope_key_normalization_does_not_clobber_jq_builtins() {
    // `any(` is followed by `(`, which is excluded from the normalization
    // boundary set, so a jq builtin call is never mistaken for a bare
    // scope-key field reference. This expression already has its leading
    // dot; it must keep evaluating exactly as jq intends.
    let scope = json!({ "item": { "xs": [1, 2, 3] } });
    assert_eq!(
        evaluate(&json!("=.item.xs | any(. > 2)"), &scope),
        json!(true)
    );
}

#[test]
fn bare_scope_key_with_pipe_only() {
    // `items | length` (no leading dot): `items` is a top-level scope key
    // and the boundary char is a space, so it normalizes to
    // `.items | length` and counts the array.
    let scope = json!({ "items": [1, 2, 3, 4] });
    assert_eq!(evaluate(&json!("=items | length"), &scope), json!(4));
}

#[test]
fn bare_ident_not_in_scope_stays_as_is() {
    // `foobar` is not a top-level key of scope, so normalization leaves
    // the program untouched; it still fails to compile as jq (bare
    // `foobar` is an undefined function) and yields Null, same as before
    // this fix.
    let scope = json!({ "item": {} });
    assert_eq!(evaluate(&json!("=foobar.x | length"), &scope), Value::Null);
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
