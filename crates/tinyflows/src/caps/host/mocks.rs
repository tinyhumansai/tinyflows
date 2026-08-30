//! Capability stand-ins for dry runs.
//!
//! The engine ships mocks that echo their request back. That is enough to prove
//! a graph *executes*, but not that it is correct: a node declaring an
//! `output_parser.schema` will have its echoed response fail validation, so a
//! perfectly good graph fails a simulation for a reason that has nothing to do
//! with the graph. A sibling host implementation hit exactly that and answered
//! it with schema-aware mocks; these are the same idea.
//!
//! A dry run therefore means: every expression resolved, every node's declared
//! output shape was satisfiable, and nothing left the process.

use crate::caps::{AgentRunner, LlmProvider};
use crate::error::Result;
use async_trait::async_trait;
use serde_json::{Value, json};

// The sample synthesizer moved to [`crate::caps::schema`]: the `testkit`
// auto-mock needs the same function and should not have to enable `host-caps`
// — and so pull in a process runner and an HTTP client — to reach it.
// Re-exported here because this is where callers have always found it.
pub use crate::caps::sample_for_schema;

/// The `output_parser.schema` a request declares, if any.
fn declared_schema(request: &Value) -> Option<&Value> {
    request.get("output_parser")?.get("schema")
}

/// The response a schema-aware mock returns for `request`.
fn mock_response(request: &Value, source: &str) -> Value {
    match declared_schema(request) {
        Some(schema) => {
            let sample = sample_for_schema(schema);
            json!({
                "text": serde_json::to_string(&sample).unwrap_or_default(),
                "json": sample,
                "mock": source,
            })
        }
        None => json!({
            "text": format!("[{source} dry run]"),
            "json": Value::Null,
            "mock": source,
        }),
    }
}

/// An [`LlmProvider`] whose response satisfies the node's declared schema.
pub struct SchemaAwareMockLlm;

#[async_trait]
impl LlmProvider for SchemaAwareMockLlm {
    async fn complete(&self, request: Value, _conn: Option<&str>) -> Result<Value> {
        Ok(mock_response(&request, "llm"))
    }
}

/// An [`AgentRunner`] whose response satisfies the node's declared schema.
///
/// Dispatches nothing: the whole point of a dry run is that no harness session
/// is started and no repository is touched.
pub struct SchemaAwareMockAgentRunner;

#[async_trait]
impl AgentRunner for SchemaAwareMockAgentRunner {
    async fn run_agent(
        &self,
        agent_ref: &str,
        request: Value,
        _conn: Option<&str>,
    ) -> Result<Value> {
        let mut response = mock_response(&request, "agent");
        if let Some(object) = response.as_object_mut() {
            // Recorded so a dry run's output shows *which* worker each node
            // would have gone to — the thing an author most often gets wrong.
            object.insert("agent_ref".into(), Value::String(agent_ref.to_string()));
        }
        Ok(response)
    }
}

#[cfg(test)]
#[path = "mocks_tests.rs"]
mod tests;
