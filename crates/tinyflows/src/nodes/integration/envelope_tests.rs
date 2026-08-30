use super::*;

#[test]
fn wraps_a_structured_object() {
    let env = wrap(json!({ "answer": 42 }));
    assert_eq!(env["json"], json!({ "answer": 42 }));
    assert_eq!(env["text"], Value::Null);
    assert_eq!(env["raw"], json!({ "answer": 42 }));
}

#[test]
fn wraps_a_bare_string_as_text() {
    let env = wrap(json!("hello world"));
    assert_eq!(env["json"], Value::Null);
    assert_eq!(env["text"], json!("hello world"));
    assert_eq!(env["raw"], json!("hello world"));
}

#[test]
fn extracts_text_field_from_an_object() {
    let env = wrap(json!({ "text": "hi", "meta": 1 }));
    // Both accessors are available: the prose via `text`, the object via `json`.
    assert_eq!(env["text"], json!("hi"));
    assert_eq!(env["json"], json!({ "text": "hi", "meta": 1 }));
}

#[test]
fn scalar_non_string_has_null_json_and_text() {
    let env = wrap(json!(7));
    assert_eq!(env["json"], Value::Null);
    assert_eq!(env["text"], Value::Null);
    assert_eq!(env["raw"], json!(7));
}

#[test]
fn from_parts_keeps_structured_and_raw_distinct() {
    // Agent case: json is the coerced value, raw is the original completion.
    let env = from_parts(
        json!({ "name": "fixed" }),
        Some("original prose".into()),
        json!({ "wrong": 1 }),
    );
    assert_eq!(env["json"], json!({ "name": "fixed" }));
    assert_eq!(env["text"], json!("original prose"));
    assert_eq!(env["raw"], json!({ "wrong": 1 }));
}

#[test]
fn text_key_is_always_present_even_when_null() {
    let env = wrap(json!({ "x": 1 }));
    assert!(env.as_object().unwrap().contains_key("text"));
    assert_eq!(env["text"], Value::Null);
}
