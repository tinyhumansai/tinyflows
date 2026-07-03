//! In-memory mock capability implementations for tests and examples.
//!
//! Enabled inside this crate's own tests automatically, or downstream via the
//! `mock` cargo feature. The mocks are deterministic echoes — enough to exercise
//! the engine and the reference workflows without any external services.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::caps::{
    Capabilities, CodeLanguage, CodeRunner, HttpClient, LlmProvider, StateStore, ToolInvoker,
};
use crate::error::Result;

/// An [`LlmProvider`] that echoes the request back under a `completion` key.
#[derive(Debug, Default, Clone)]
pub struct MockLlm;

#[async_trait]
impl LlmProvider for MockLlm {
    async fn complete(&self, request: Value, conn: Option<&str>) -> Result<Value> {
        Ok(json!({ "completion": request, "connection": conn }))
    }
}

/// A [`ToolInvoker`] that echoes the slug and args it was called with.
#[derive(Debug, Default, Clone)]
pub struct MockTools;

#[async_trait]
impl ToolInvoker for MockTools {
    async fn invoke(&self, slug: &str, args: Value, conn: Option<&str>) -> Result<Value> {
        Ok(json!({ "tool": slug, "args": args, "connection": conn }))
    }
}

/// An [`HttpClient`] that returns a canned `200` response echoing the request.
#[derive(Debug, Default, Clone)]
pub struct MockHttp;

#[async_trait]
impl HttpClient for MockHttp {
    async fn request(&self, request: Value, conn: Option<&str>) -> Result<Value> {
        Ok(json!({ "status": 200, "request": request, "connection": conn }))
    }
}

/// A [`CodeRunner`] that returns its input unchanged under a `result` key.
#[derive(Debug, Default, Clone)]
pub struct MockCode;

#[async_trait]
impl CodeRunner for MockCode {
    async fn run(&self, _language: CodeLanguage, _source: &str, input: Value) -> Result<Value> {
        Ok(json!({ "result": input }))
    }
}

/// A [`StateStore`] backed by an in-memory map guarded by a mutex.
#[derive(Debug, Default)]
pub struct MockStateStore {
    inner: std::sync::Mutex<std::collections::HashMap<String, Value>>,
}

#[async_trait]
impl StateStore for MockStateStore {
    async fn load(&self, key: &str) -> Result<Option<Value>> {
        Ok(self.inner.lock().expect("lock").get(key).cloned())
    }

    async fn store(&self, key: &str, value: Value) -> Result<()> {
        self.inner
            .lock()
            .expect("lock")
            .insert(key.to_string(), value);
        Ok(())
    }
}

/// Builds a [`Capabilities`] bundle wired entirely to the mock implementations.
#[must_use]
pub fn mock_capabilities() -> Capabilities {
    Capabilities {
        llm: Arc::new(MockLlm),
        tools: Arc::new(MockTools),
        http: Arc::new(MockHttp),
        code: Arc::new(MockCode),
        state: Arc::new(MockStateStore::default()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_tools_echo() {
        let caps = mock_capabilities();
        let out = caps
            .tools
            .invoke("slack.post", json!({"x": 1}), None)
            .await
            .unwrap();
        assert_eq!(out["tool"], "slack.post");
    }

    #[tokio::test]
    async fn mock_llm_echoes_request_and_threads_connection() {
        let llm = MockLlm;
        let with_conn = llm
            .complete(json!({"prompt": "hi"}), Some("conn_1"))
            .await
            .unwrap();
        assert_eq!(with_conn["completion"], json!({"prompt": "hi"}));
        assert_eq!(with_conn["connection"], "conn_1");

        let without_conn = llm.complete(json!({"prompt": "hi"}), None).await.unwrap();
        assert!(without_conn["connection"].is_null());
    }

    #[tokio::test]
    async fn mock_tools_echoes_slug_args_and_connection() {
        let tools = MockTools;
        let with_conn = tools
            .invoke("gmail.send", json!({"to": "a@b.c"}), Some("conn_2"))
            .await
            .unwrap();
        assert_eq!(with_conn["tool"], "gmail.send");
        assert_eq!(with_conn["args"], json!({"to": "a@b.c"}));
        assert_eq!(with_conn["connection"], "conn_2");

        let without_conn = tools.invoke("gmail.send", json!({}), None).await.unwrap();
        assert!(without_conn["connection"].is_null());
    }

    #[tokio::test]
    async fn mock_http_returns_canned_200_and_threads_connection() {
        let http = MockHttp;
        let with_conn = http
            .request(json!({"method": "GET", "url": "https://x"}), Some("conn_3"))
            .await
            .unwrap();
        assert_eq!(with_conn["status"], 200);
        assert_eq!(with_conn["request"]["method"], "GET");
        assert_eq!(with_conn["connection"], "conn_3");

        let without_conn = http.request(json!({"method": "GET"}), None).await.unwrap();
        assert_eq!(without_conn["status"], 200);
        assert!(without_conn["connection"].is_null());
    }

    #[tokio::test]
    async fn mock_code_returns_input_under_result_key() {
        let code = MockCode;
        let js = code
            .run(CodeLanguage::JavaScript, "return 1;", json!({"n": 7}))
            .await
            .unwrap();
        assert_eq!(js["result"], json!({"n": 7}));

        // The language and source are ignored by the echo runner.
        let py = code
            .run(CodeLanguage::Python, "print(1)", json!([1, 2, 3]))
            .await
            .unwrap();
        assert_eq!(py["result"], json!([1, 2, 3]));
    }

    #[tokio::test]
    async fn mock_state_store_round_trips_and_misses() {
        let store = MockStateStore::default();
        assert!(store.load("missing").await.unwrap().is_none());

        store.store("k", json!({"v": 1})).await.unwrap();
        assert_eq!(store.load("k").await.unwrap(), Some(json!({"v": 1})));

        // Storing again overwrites the previous value.
        store.store("k", json!(2)).await.unwrap();
        assert_eq!(store.load("k").await.unwrap(), Some(json!(2)));
    }

    #[tokio::test]
    async fn mock_capabilities_state_store_round_trips_through_bundle() {
        let caps = mock_capabilities();
        // A missing key reads back as `None`.
        assert!(caps.state.load("k").await.unwrap().is_none());

        // A stored value is readable through the same bundle handle.
        caps.state.store("k", json!({"v": 1})).await.unwrap();
        assert_eq!(caps.state.load("k").await.unwrap(), Some(json!({"v": 1})));
    }

    #[tokio::test]
    async fn mock_capabilities_wires_every_slot() {
        let caps = mock_capabilities();
        assert_eq!(
            caps.llm.complete(json!({"p": 1}), None).await.unwrap()["completion"],
            json!({"p": 1})
        );
        assert_eq!(
            caps.tools.invoke("s", json!({}), None).await.unwrap()["tool"],
            "s"
        );
        assert_eq!(
            caps.http.request(json!({}), None).await.unwrap()["status"],
            200
        );
        assert_eq!(
            caps.code
                .run(CodeLanguage::Python, "", json!("x"))
                .await
                .unwrap()["result"],
            "x"
        );
    }
}
