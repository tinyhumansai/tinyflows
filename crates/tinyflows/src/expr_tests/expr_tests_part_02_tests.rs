
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
