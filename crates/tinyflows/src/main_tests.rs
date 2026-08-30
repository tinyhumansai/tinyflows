use std::panic::{AssertUnwindSafe, catch_unwind};

use super::*;

#[tokio::test]
async fn poisoned_memory_state_is_an_error_not_a_missing_key() {
    let state = MemoryState::default();
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let _guard = state.0.lock().unwrap();
        panic!("poison state lock");
    }));

    assert!(state.load("missing").await.is_err());
    assert!(state.store("key", Value::Null).await.is_err());
}
