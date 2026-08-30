use super::*;

/// The plain behaviour the sentinel sits alongside: objects gain keys.
#[test]
fn objects_merge_key_by_key_and_scalars_overwrite() {
    let mut base = json!({ "a": 1, "nested": { "x": 1 } });
    merge(&mut base, json!({ "b": 2, "nested": { "y": 2 } }));
    assert_eq!(
        base,
        json!({ "a": 1, "b": 2, "nested": { "x": 1, "y": 2 } })
    );
}

/// The problem the sentinel solves: without it, a key can never be dropped.
#[test]
fn a_plain_merge_cannot_remove_a_key_but_replace_can() {
    let mut base = json!({ "attempts": [1], "err": "boom" });
    merge(&mut base, json!({ "attempts": [1, 2] }));
    assert_eq!(
        base["err"], "boom",
        "a plain merge leaves the key it did not mention"
    );

    let mut base = json!({ "attempts": [1], "err": "boom" });
    merge(&mut base, replace(json!({ "attempts": [1, 2] })));
    assert_eq!(
        base,
        json!({ "attempts": [1, 2] }),
        "replace assigns wholesale, so the dropped key is gone"
    );
}

#[test]
fn replace_works_at_every_nesting_depth() {
    let mut base = json!({ "nodes": { "l": { "state": { "a": 1, "b": 2 } } } });
    merge(
        &mut base,
        json!({ "nodes": { "l": { "state": replace(json!({ "a": 9 })) } } }),
    );
    assert_eq!(base["nodes"]["l"]["state"], json!({ "a": 9 }));

    let mut base = json!({ "x": 1 });
    merge(&mut base, replace(json!("scalar")));
    assert_eq!(base, json!("scalar"), "replace works at the root too");
}

/// The soundness argument, as a test: `merge` never walks into an items
/// array, so a workflow whose *data* contains a `$replace` key is never
/// examined by the sentinel check and cannot trigger it.
#[test]
fn a_replace_key_inside_item_data_is_left_alone() {
    let payload = json!({ REPLACE: "user data, not a sentinel" });
    let mut base = json!({ "nodes": { "n": { "items": [] } } });
    merge(
        &mut base,
        json!({ "nodes": { "n": { "items": [ { "json": payload.clone() } ] } } }),
    );
    assert_eq!(
        base["nodes"]["n"]["items"][0]["json"], payload,
        "item payloads ride inside an array and are copied verbatim"
    );
}

/// Only an update that is *exactly* the sentinel assigns. An object that
/// merely contains the key alongside others is ordinary data.
#[test]
fn only_a_lone_replace_key_is_treated_as_the_sentinel() {
    let mut base = json!({ "keep": 1 });
    merge(&mut base, json!({ REPLACE: "x", "other": 2 }));
    assert_eq!(
        base,
        json!({ "keep": 1, REPLACE: "x", "other": 2 }),
        "a two-key object is data, and merges normally"
    );
}
