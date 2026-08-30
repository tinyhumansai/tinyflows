use super::*;
use serde_json::json;

fn spec() -> TaskSpec {
    TaskSpec::Tool {
        slug: "demo.run".to_string(),
        args: json!({ "x": 1 }),
    }
}

#[tokio::test]
async fn a_started_task_settles_and_can_be_polled_repeatedly() {
    let runner = TokioTaskRunner::new();
    let ticket = runner.start(spec()).await.expect("start");

    // Poll until settled; the gate does exactly this, once per activation.
    let mut state = runner.poll(&ticket).await.expect("poll");
    for _ in 0..64 {
        if state.is_settled() {
            break;
        }
        tokio::task::yield_now().await;
        state = runner.poll(&ticket).await.expect("poll");
    }
    assert!(
        matches!(state, TaskState::Done(_)),
        "task should settle, got {state:?}"
    );
    // Polling again must not consume the result — a gate may see the same
    // ticket on several activations before it releases.
    assert_eq!(runner.poll(&ticket).await.expect("re-poll"), state);
}

#[tokio::test]
async fn tickets_are_unique() {
    let runner = TokioTaskRunner::new();
    let a = runner.start(spec()).await.expect("start");
    let b = runner.start(spec()).await.expect("start");
    assert_ne!(a, b);
}

#[tokio::test]
async fn an_unknown_ticket_is_an_error_rather_than_a_silent_pending() {
    let runner = TokioTaskRunner::new();
    assert!(runner.poll("task-999").await.is_err());
    assert!(runner.cancel("task-999").await.is_err());
}

#[tokio::test]
async fn cancelling_settles_the_task_as_failed() {
    let runner = TokioTaskRunner::new();
    let ticket = runner.start(spec()).await.expect("start");
    runner.cancel(&ticket).await.expect("cancel");
    let state = runner.poll(&ticket).await.expect("poll");
    assert!(state.is_settled(), "a cancelled task must not stay pending");
}

/// Cancelling something that already finished must not rewrite its result —
/// a gate that released on it has already used that value.
#[tokio::test]
async fn cancelling_a_settled_task_leaves_its_result_alone() {
    let runner = TokioTaskRunner::new();
    let ticket = runner.start(spec()).await.expect("start");
    for _ in 0..64 {
        if runner.poll(&ticket).await.expect("poll").is_settled() {
            break;
        }
        tokio::task::yield_now().await;
    }
    let before = runner.poll(&ticket).await.expect("poll");
    runner.cancel(&ticket).await.expect("cancel");
    assert_eq!(runner.poll(&ticket).await.expect("poll"), before);
}
