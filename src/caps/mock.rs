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
    async fn complete(&self, request: Value) -> Result<Value> {
        Ok(json!({ "completion": request }))
    }
}

/// A [`ToolInvoker`] that echoes the slug and args it was called with.
#[derive(Debug, Default, Clone)]
pub struct MockTools;

#[async_trait]
impl ToolInvoker for MockTools {
    async fn invoke(&self, slug: &str, args: Value) -> Result<Value> {
        Ok(json!({ "tool": slug, "args": args }))
    }
}

/// An [`HttpClient`] that returns a canned `200` response echoing the request.
#[derive(Debug, Default, Clone)]
pub struct MockHttp;

#[async_trait]
impl HttpClient for MockHttp {
    async fn request(&self, request: Value) -> Result<Value> {
        Ok(json!({ "status": 200, "request": request }))
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
            .invoke("slack.post", json!({"x": 1}))
            .await
            .unwrap();
        assert_eq!(out["tool"], "slack.post");
    }
}
