use super::*;

#[test]
fn cancel_build_turn_scoped_cancels_a_registered_token() {
    let thread_id = "build-registry-test:thread-1";
    let token = CancellationToken::new();
    register_build_turn(
        thread_id.to_string(),
        Some("req-1".to_string()),
        token.clone(),
    );

    assert!(!token.is_cancelled());
    assert!(cancel_build_turn_scoped(thread_id, Some("req-1")));
    assert!(token.is_cancelled());

    unregister_build_turn(thread_id, Some("req-1"));
}

#[test]
fn cancel_build_turn_scoped_returns_false_when_nothing_registered() {
    assert!(!cancel_build_turn_scoped(
        "build-registry-test:never-registered",
        Some("req-x")
    ));
    assert!(!cancel_build_turn_scoped(
        "build-registry-test:never-registered",
        None
    ));
}

#[test]
fn cancel_build_turn_scoped_ignores_a_mismatched_request_id() {
    let thread_id = "build-registry-test:thread-2";
    let token = CancellationToken::new();
    register_build_turn(
        thread_id.to_string(),
        Some("req-1".to_string()),
        token.clone(),
    );

    // A stale/unrelated request_id must not cancel a still-live turn.
    assert!(!cancel_build_turn_scoped(
        thread_id,
        Some("some-other-request")
    ));
    assert!(!token.is_cancelled());

    // The real request_id still works.
    assert!(cancel_build_turn_scoped(thread_id, Some("req-1")));
    assert!(token.is_cancelled());

    unregister_build_turn(thread_id, Some("req-1"));
}

#[test]
fn cancel_build_turn_scoped_unscoped_none_cancels_whatever_is_registered() {
    let thread_id = "build-registry-test:thread-3";
    let token = CancellationToken::new();
    register_build_turn(
        thread_id.to_string(),
        Some("req-1".to_string()),
        token.clone(),
    );

    assert!(cancel_build_turn_scoped(thread_id, None));
    assert!(token.is_cancelled());

    unregister_build_turn(thread_id, None);
}

#[test]
fn unregister_build_turn_is_match_guarded() {
    let thread_id = "build-registry-test:thread-4";
    let token_a = CancellationToken::new();
    register_build_turn(
        thread_id.to_string(),
        Some("req-a".to_string()),
        token_a.clone(),
    );

    // A newer turn supersedes the entry (mirrors `flows_build` starting a
    // second turn on the same thread before the first one's cleanup ran).
    // Registering it must cancel the displaced `token_a` — a second build
    // starting while the first is still genuinely running must not leave the
    // first one running unreachable by Stop.
    let token_b = CancellationToken::new();
    register_build_turn(
        thread_id.to_string(),
        Some("req-b".to_string()),
        token_b.clone(),
    );
    assert!(
        token_a.is_cancelled(),
        "the displaced turn's token must be cancelled when a newer turn replaces it"
    );
    assert!(!token_b.is_cancelled());

    // An unregister scoped to the OLD (superseded) request must not clobber
    // the newer turn's entry.
    unregister_build_turn(thread_id, Some("req-a"));
    assert!(
        cancel_build_turn_scoped(thread_id, Some("req-b")),
        "the newer turn's entry must still be registered after a stale unregister"
    );
    assert!(token_b.is_cancelled());

    unregister_build_turn(thread_id, Some("req-b"));
    assert!(!cancel_build_turn_scoped(thread_id, None));
}

#[test]
fn unregister_build_turn_none_removes_unconditionally() {
    let thread_id = "build-registry-test:thread-5";
    let token = CancellationToken::new();
    register_build_turn(thread_id.to_string(), Some("req-1".to_string()), token);

    unregister_build_turn(thread_id, None);
    assert!(!cancel_build_turn_scoped(thread_id, None));
}
