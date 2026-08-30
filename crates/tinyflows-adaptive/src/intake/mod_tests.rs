use super::*;

#[test]
fn a_bare_object_is_read_as_itself() {
    let answer = serde_json::json!({ "workflow_id": "pr-review" });
    assert_eq!(extract(&answer).unwrap()["workflow_id"], "pr-review");
}

#[test]
fn an_openai_shaped_envelope_is_unwrapped() {
    let answer = serde_json::json!({
        "choices": [{ "message": { "content": "{\"workflow_id\":\"pr-review\"}" } }]
    });
    assert_eq!(extract(&answer).unwrap()["workflow_id"], "pr-review");
}

#[test]
fn a_text_field_holding_json_is_read() {
    let answer = serde_json::json!({ "text": "{\"workflow_id\":\"x\"}" });
    assert_eq!(extract(&answer).unwrap()["workflow_id"], "x");
}

#[test]
fn prose_around_the_object_does_not_lose_it() {
    // Models do this whatever the response_format asked for.
    let answer = serde_json::json!({
        "text": "Sure! Here you go:\n```json\n{\"workflow_id\":\"x\"}\n```\nHope that helps."
    });
    assert_eq!(extract(&answer).unwrap()["workflow_id"], "x");
}

#[test]
fn an_answer_with_no_object_at_all_is_none_rather_than_a_panic() {
    assert!(extract(&serde_json::json!({ "text": "I could not decide." })).is_none());
    assert!(extract(&serde_json::json!("just a string")).is_none());
}
