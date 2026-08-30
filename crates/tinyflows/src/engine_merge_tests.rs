//! Property tests for the workflow-state reducer.

use proptest::prelude::*;
use serde_json::{Map, Value, json};

use super::{merge, replace};

fn leaf() -> impl Strategy<Value = Value> {
    prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i32>().prop_map(|value| json!(value)),
        ".{0,16}".prop_map(Value::String),
        prop::collection::vec(any::<i16>(), 0..8).prop_map(|values| json!(values)),
    ]
}

fn arbitrary_json() -> impl Strategy<Value = Value> {
    leaf().prop_recursive(4, 96, 8, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..6).prop_map(Value::Array),
            prop::collection::btree_map("[a-z]{1,5}", inner, 0..6)
                .prop_map(|entries| Value::Object(entries.into_iter().collect())),
        ]
    })
}

/// Real engine updates have a stable object shape down to the leaf being
/// assigned. Generating that shape avoids meaningless type-conflict examples
/// (`scalar` followed by `object`) that the engine never emits.
fn compatible_update() -> impl Strategy<Value = Value> {
    (0usize..5, 0usize..5, leaf()).prop_map(|(node, field, value)| {
        json!({ "nodes": { format!("n{node}"): { format!("k{field}"): value } } })
    })
}

fn folded(initial: Value, updates: impl IntoIterator<Item = Value>) -> Value {
    let mut state = initial;
    for update in updates {
        merge(&mut state, update);
    }
    state
}

fn nested_update(path: &[String], value: Value) -> Value {
    path.iter()
        .rev()
        .fold(replace(value), |child, key| json!({ key: child }))
}

fn assign_at_path(root: &mut Value, path: &[String], value: Value) {
    if path.is_empty() {
        *root = value;
        return;
    }
    if !root.is_object() {
        *root = Value::Object(Map::new());
    }
    let child = root
        .as_object_mut()
        .expect("object created above")
        .entry(path[0].clone())
        .or_insert(Value::Null);
    assign_at_path(child, &path[1..], value);
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    #[test]
    fn compatible_update_sequences_are_associative(
        run_state in arbitrary_json(),
        updates in prop::collection::vec(compatible_update(), 0..40),
        split in 0usize..40,
    ) {
        let initial = json!({ "run": run_state, "nodes": {} });
        let split = split.min(updates.len());
        let sequential = folded(initial.clone(), updates.clone());

        let left = folded(Value::Object(Map::new()), updates[..split].iter().cloned());
        let right = folded(Value::Object(Map::new()), updates[split..].iter().cloned());
        let grouped = folded(initial, [left, right]);

        prop_assert_eq!(sequential, grouped);
    }

    #[test]
    fn replace_round_trips_at_arbitrary_nesting(
        base in arbitrary_json(),
        replacement in arbitrary_json(),
        path in prop::collection::vec("[a-z]{1,5}", 0..7),
    ) {
        let mut actual = base.clone();
        merge(&mut actual, nested_update(&path, replacement.clone()));

        let mut expected = base;
        assign_at_path(&mut expected, &path, replacement);
        prop_assert_eq!(actual, expected);
    }
}
