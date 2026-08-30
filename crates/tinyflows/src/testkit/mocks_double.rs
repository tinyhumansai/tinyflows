//! [`Double`], the single type that implements every capability trait for
//! [`MockCaps`](super::MockCaps) by consulting its rules, recording what
//! happened to [`CallLog`](super::log::CallLog), and answering.
//!
//! Split out of `mocks.rs` to keep that file under the repository's
//! line-length limit.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::caps::{
    AgentRunner, ApprovalDecision, ApprovalOutcome, ApprovalProvider, ApprovalRequest,
    CodeLanguage, CodeRunner, HttpClient, LlmProvider, MemoryProvider, ShellOutcome, ShellRequest,
    ShellRunner, StateStore, ToolInvoker, WorkflowResolver,
};
use crate::error::{EngineError, Result};
use crate::model::WorkflowGraph;

use super::log::CallOutcome;
use super::{MockCaps, capability};

/// One capability double: it consults the rules, records the call, and answers.
///
/// A single type implementing every capability trait rather than nine, because
/// each implementation is the same three steps and nine copies of them would
/// drift.
pub(super) struct Double {
    mocks: Arc<MockCaps>,
    /// The node this double was scoped to, stamped onto every call it logs.
    node_id: Option<String>,
}

impl Double {
    pub(super) fn new(mocks: Arc<MockCaps>, node_id: Option<String>) -> Self {
        Self { mocks, node_id }
    }

    /// Consult the rules, log whatever happens, and return it.
    async fn dispatch(
        &self,
        capability: &str,
        method: &str,
        target: String,
        request: Value,
        default: impl FnOnce(&Value) -> Value,
    ) -> Result<Value> {
        let programmed = self
            .mocks
            .respond_to(capability, &target, self.node_id.as_deref(), &request)
            .await;
        let result = match programmed {
            Some(result) => result,
            None => Ok(default(&request)),
        };
        let outcome = match &result {
            Ok(value) => CallOutcome::Ok(value.clone()),
            Err(err) => CallOutcome::Err(err.to_string()),
        };
        self.mocks.log().record(
            capability,
            method,
            self.node_id.clone(),
            target,
            request,
            outcome,
        );
        result
    }
}

#[async_trait]
impl LlmProvider for Double {
    async fn complete(&self, request: Value, conn: Option<&str>) -> Result<Value> {
        let conn = conn.map(str::to_string);
        self.dispatch(
            capability::LLM,
            "complete",
            String::new(),
            request,
            |req| json!({ "completion": req, "connection": conn }),
        )
        .await
    }
}

#[async_trait]
impl ToolInvoker for Double {
    async fn invoke(&self, slug: &str, args: Value, conn: Option<&str>) -> Result<Value> {
        let slug_owned = slug.to_string();
        let conn = conn.map(str::to_string);
        self.dispatch(
            capability::TOOLS,
            "invoke",
            slug.to_string(),
            args,
            move |args| json!({ "tool": slug_owned, "args": args, "connection": conn }),
        )
        .await
    }
}

#[async_trait]
impl HttpClient for Double {
    async fn request(&self, request: Value, conn: Option<&str>) -> Result<Value> {
        // The URL is what a rule globs on; a request without one still matches
        // a bare `*`.
        let url = request
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let conn = conn.map(str::to_string);
        self.dispatch(
            capability::HTTP,
            "request",
            url,
            request,
            |req| json!({ "status": 200, "request": req, "connection": conn }),
        )
        .await
    }
}

#[async_trait]
impl CodeRunner for Double {
    async fn run(&self, language: CodeLanguage, source: &str, input: Value) -> Result<Value> {
        let request = json!({
            "language": format!("{language:?}"),
            "source": source,
            "input": input,
        });
        self.dispatch(
            capability::CODE,
            "run",
            format!("{language:?}"),
            request,
            |req| json!({ "result": req.get("input").cloned().unwrap_or(Value::Null) }),
        )
        .await
    }
}

#[async_trait]
impl ShellRunner for Double {
    async fn run(&self, request: ShellRequest) -> Result<ShellOutcome> {
        let script = match &request.script {
            crate::caps::ShellScript::Inline(source) => source.clone(),
            crate::caps::ShellScript::Path(path) => path.clone(),
        };
        let encoded = json!({
            "interpreter": request.interpreter.as_str(),
            "script": script,
            "cwd": request.cwd,
            "env": request.env,
            "input": request.input,
        });
        let value = self
            .dispatch(
                capability::SHELL,
                "run",
                script.clone(),
                encoded,
                move |_req| json!({ "exit_code": 0, "stdout": script, "stderr": "" }),
            )
            .await?;
        // A programmed value may describe the whole outcome, or just be the
        // stdout a test cares about. Accept either rather than making a caller
        // spell out an exit code they do not care about.
        Ok(ShellOutcome {
            exit_code: value
                .get("exit_code")
                .and_then(Value::as_i64)
                .unwrap_or(0)
                .try_into()
                .unwrap_or(0),
            stdout: value
                .get("stdout")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| value.to_string()),
            stderr: value
                .get("stderr")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        })
    }
}

#[async_trait]
impl AgentRunner for Double {
    async fn run_agent(
        &self,
        agent_ref: &str,
        request: Value,
        conn: Option<&str>,
    ) -> Result<Value> {
        let name = agent_ref.to_string();
        let conn = conn.map(str::to_string);
        self.dispatch(
            capability::AGENT,
            "run_agent",
            agent_ref.to_string(),
            request,
            move |req| json!({ "agent": name, "request": req, "connection": conn }),
        )
        .await
    }
}

#[async_trait]
impl MemoryProvider for Double {
    async fn recall(&self, scope: &str, query: &str, opts: Value) -> Result<Value> {
        let request = json!({ "scope": scope, "query": query, "opts": opts });
        self.dispatch(
            capability::MEMORY,
            "recall",
            scope.to_string(),
            request,
            |_| json!({ "results": [] }),
        )
        .await
    }

    async fn flavour(&self, slug: &str) -> Result<Value> {
        let request = json!({ "slug": slug });
        self.dispatch(
            capability::MEMORY,
            "flavour",
            slug.to_string(),
            request,
            |_| json!({ "traits": {} }),
        )
        .await
    }

    async fn people(&self, query: Option<&str>) -> Result<Value> {
        let request = json!({ "query": query });
        self.dispatch(
            capability::MEMORY,
            "people",
            String::new(),
            request,
            |_| json!({ "people": [] }),
        )
        .await
    }

    async fn remember(&self, scope: &str, key: &str, value: Value) -> Result<()> {
        let request = json!({ "scope": scope, "key": key, "value": value });
        self.dispatch(
            capability::MEMORY,
            "remember",
            format!("{scope}/{key}"),
            request,
            |_| Value::Null,
        )
        .await
        .map(|_| ())
    }

    async fn forget(&self, scope: &str, key: &str) -> Result<()> {
        let request = json!({ "scope": scope, "key": key });
        self.dispatch(
            capability::MEMORY,
            "forget",
            format!("{scope}/{key}"),
            request,
            |_| Value::Null,
        )
        .await
        .map(|_| ())
    }
}

#[async_trait]
impl StateStore for Double {
    async fn load(&self, key: &str) -> Result<Option<Value>> {
        let stored = self
            .mocks
            .state
            .lock()
            .expect("mock state poisoned")
            .get(key)
            .cloned();
        // Logged like any other call, but the *store* is the source of truth:
        // a rule that overrode a load would make a stateful graph unreadable.
        self.mocks.log().record(
            capability::STATE,
            "load",
            self.node_id.clone(),
            key.to_string(),
            json!({ "key": key }),
            CallOutcome::Ok(stored.clone().unwrap_or(Value::Null)),
        );
        Ok(stored)
    }

    async fn store(&self, key: &str, value: Value) -> Result<()> {
        self.mocks
            .state
            .lock()
            .expect("mock state poisoned")
            .insert(key.to_string(), value.clone());
        self.mocks.log().record(
            capability::STATE,
            "store",
            self.node_id.clone(),
            key.to_string(),
            json!({ "key": key, "value": value }),
            CallOutcome::Ok(Value::Null),
        );
        Ok(())
    }
}

#[async_trait]
impl ApprovalProvider for Double {
    async fn decide(&self, request: &ApprovalRequest) -> Result<ApprovalOutcome> {
        let encoded = json!({
            "request_id": request.request_id,
            "node_id": request.node_id,
            "run_id": request.run_id,
            "title": request.title,
            "prompt": request.prompt,
            "subject": {
                "kind": request.subject.kind,
                "value": request.subject.value,
            },
            "assignees": request.assignees,
            "metadata": request.metadata,
        });
        // An unprogrammed review approves, so a graph that contains one runs end
        // to end without a test standing a reviewer up — the same bargain every
        // other default here makes. A test that cares about the answer says so
        // with `on_approval`.
        let value = self
            .dispatch(
                capability::APPROVALS,
                "decide",
                request.request_id.clone(),
                encoded,
                |_| json!({ "approved": true, "decided_by": "testkit" }),
            )
            .await?;
        // A programmed answer may be the whole verdict or just the bit the test
        // cares about, as with `ShellRunner` above — including the bare string
        // `"pending"` `on_approval`'s own doc comment advertises as shorthand
        // for "nobody has got to this review yet".
        let is_pending = value.as_str() == Some("pending")
            || value.get("status").and_then(Value::as_str) == Some("pending");
        if is_pending {
            return Ok(ApprovalOutcome::Pending);
        }
        Ok(ApprovalOutcome::Decided(ApprovalDecision {
            approved: value
                .get("approved")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            decided_by: value
                .get("decided_by")
                .and_then(Value::as_str)
                .map(str::to_string),
            comment: value
                .get("comment")
                .and_then(Value::as_str)
                .map(str::to_string),
            payload: value.get("payload").cloned(),
        }))
    }

    async fn cancel(&self, request_id: &str, reason: &str) -> Result<()> {
        self.dispatch(
            capability::APPROVALS,
            "cancel",
            request_id.to_string(),
            json!({ "request_id": request_id, "reason": reason }),
            |_| json!({ "cancelled": true }),
        )
        .await
        .map(|_| ())
    }
}

#[async_trait]
impl WorkflowResolver for Double {
    async fn resolve(&self, workflow_id: &str) -> Result<WorkflowGraph> {
        self.mocks
            .workflows
            .get(workflow_id)
            .cloned()
            .ok_or_else(|| {
                EngineError::Capability(format!(
                    "testkit: no workflow registered as {workflow_id:?}"
                ))
            })
    }
}
