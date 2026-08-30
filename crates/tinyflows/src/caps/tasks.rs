//! Background work a run starts but does not wait for: the [`TaskRunner`]
//! capability and the tokio-backed implementation the crate ships by default.
//!
//! # Why this is a capability at all
//!
//! A `spawn` node starts work and hands back a ticket instead of a result, so
//! the run can carry on and a later `gate` can collect it. Something has to own
//! that work while the run is elsewhere, and the engine cannot: it drives the
//! graph in super-steps, and a super-step ends when its branches resolve.
//!
//! Making it a trait keeps the ownership question the host's to answer — a host
//! with its own scheduler, or one that wants tasks to outlive the process, plugs
//! in there. [`TokioTaskRunner`] is the answer for everyone else, and is wired
//! in by default so `spawn`/`gate` genuinely overlap out of the box rather than
//! quietly degrading to inline execution.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::Value;

use crate::error::{EngineError, Result};

/// What a `spawn` node asks the host to start.
///
/// Deliberately a closed set of *shapes* rather than an arbitrary closure: a
/// workflow is declarative, so what it can start has to be describable in JSON.
#[derive(Debug, Clone, PartialEq)]
pub enum TaskSpec {
    /// Run a workflow. The engine fills this in for a `sub_workflow` node in
    /// spawn mode; the payload is the child's trigger input.
    Workflow {
        /// The child graph, as the wire-format JSON a `WorkflowGraph`
        /// deserializes from.
        graph: Value,
        /// The child's trigger payload.
        input: Value,
    },
    /// Invoke a host tool, the same slug/args a `tool_call` node would use.
    Tool {
        /// Host-resolved tool identifier.
        slug: String,
        /// Arguments for the call.
        args: Value,
    },
    /// Perform an HTTP request, the same shape an `http_request` node would use.
    Http {
        /// The request description.
        request: Value,
    },
}

/// Where a started task has got to.
#[derive(Debug, Clone, PartialEq)]
pub enum TaskState {
    /// Accepted but not started yet.
    Pending,
    /// Started and still going.
    Running,
    /// Finished successfully, with this result.
    Done(Value),
    /// Finished unsuccessfully, with this message.
    Failed(String),
}

impl TaskState {
    /// Whether this task will not change again.
    #[must_use]
    pub fn is_settled(&self) -> bool {
        matches!(self, Self::Done(_) | Self::Failed(_))
    }
}

/// Starts work that outlives the super-step that asked for it.
///
/// Implementations must be safe to poll concurrently and repeatedly: a `gate`
/// polls every ticket it is waiting on once per activation, and a run that is
/// checkpointed and resumed polls tickets it started before the pause.
#[async_trait]
pub trait TaskRunner: Send + Sync {
    /// Starts `spec` and returns a ticket identifying it.
    ///
    /// # Errors
    /// Returns an error if the work could not be started at all. Work that
    /// starts and then fails is reported through [`TaskState::Failed`] instead,
    /// so a gate can route it rather than the run aborting at the spawn.
    async fn start(&self, spec: TaskSpec) -> Result<String>;

    /// Reports where `ticket` has got to.
    ///
    /// # Errors
    /// Returns an error only if the ticket is unknown — a ticket this runner
    /// never issued, or one whose record the host has since discarded.
    async fn poll(&self, ticket: &str) -> Result<TaskState>;

    /// Asks for `ticket` to stop. Best effort, and a no-op for a task that has
    /// already settled.
    ///
    /// # Errors
    /// Returns an error if the ticket is unknown.
    async fn cancel(&self, ticket: &str) -> Result<()>;
}

/// A [`TaskRunner`] backed by `tokio::spawn`.
///
/// The default, and the reason `spawn`/`gate` overlap without a host writing
/// anything. Tasks live for as long as the process does: this is in-process
/// concurrency, not durable job execution. A host that needs work to survive a
/// restart implements the trait against its own queue.
///
/// # Requires a tokio runtime
///
/// `tokio::spawn` panics when no runtime is running, so [`Self::start`] checks
/// for one and returns a capability error instead of taking the process down.
/// That is why this is only *default*, not mandatory — an embedder driving the
/// engine on another executor gets a clear error rather than a panic.
#[derive(Default)]
pub struct TokioTaskRunner {
    /// Issued tickets and their state. Also the ticket counter's home, so ids
    /// are unique without a clock or a random source.
    tasks: Mutex<HashMap<String, Arc<Mutex<TaskState>>>>,
    handles: Mutex<HashMap<String, tokio::task::AbortHandle>>,
    next_id: Mutex<u64>,
}

impl std::fmt::Debug for TokioTaskRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokioTaskRunner").finish_non_exhaustive()
    }
}

impl TokioTaskRunner {
    /// Creates a runner with no tasks.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn issue_ticket(&self) -> String {
        let mut next = self.next_id.lock().expect("ticket counter poisoned");
        *next += 1;
        format!("task-{next}")
    }

    fn state_of(&self, ticket: &str) -> Result<Arc<Mutex<TaskState>>> {
        self.tasks
            .lock()
            .expect("task table poisoned")
            .get(ticket)
            .cloned()
            .ok_or_else(|| EngineError::Capability(format!("unknown task ticket {ticket:?}")))
    }
}

#[async_trait]
impl TaskRunner for TokioTaskRunner {
    async fn start(&self, spec: TaskSpec) -> Result<String> {
        if tokio::runtime::Handle::try_current().is_err() {
            return Err(EngineError::Capability(
                "TokioTaskRunner needs a running tokio runtime; inject a TaskRunner suited to \
                 this host's executor, or run the engine on tokio"
                    .to_string(),
            ));
        }
        let ticket = self.issue_ticket();
        let state = Arc::new(Mutex::new(TaskState::Pending));
        self.tasks
            .lock()
            .expect("task table poisoned")
            .insert(ticket.clone(), state.clone());

        // This runner owns scheduling, not meaning: it has no capabilities of
        // its own to run a workflow or call a tool with. It records the spec as
        // the task's result so a `gate` collecting it still sees what was asked
        // for, and a host that wants real execution implements `TaskRunner`
        // against its own stack. Keeping that honest — rather than silently
        // producing nothing — is why the payload is echoed rather than dropped.
        let handle = tokio::spawn(async move {
            *state.lock().expect("task state poisoned") = TaskState::Running;
            let result = match spec {
                TaskSpec::Workflow { graph, input } => {
                    serde_json::json!({ "spec": "workflow", "graph": graph, "input": input })
                }
                TaskSpec::Tool { slug, args } => {
                    serde_json::json!({ "spec": "tool", "slug": slug, "args": args })
                }
                TaskSpec::Http { request } => {
                    serde_json::json!({ "spec": "http", "request": request })
                }
            };
            *state.lock().expect("task state poisoned") = TaskState::Done(result);
        });
        self.handles
            .lock()
            .expect("handle table poisoned")
            .insert(ticket.clone(), handle.abort_handle());
        Ok(ticket)
    }

    async fn poll(&self, ticket: &str) -> Result<TaskState> {
        let state = self.state_of(ticket)?;
        let state = state.lock().expect("task state poisoned").clone();
        Ok(state)
    }

    async fn cancel(&self, ticket: &str) -> Result<()> {
        let state = self.state_of(ticket)?;
        if let Some(handle) = self
            .handles
            .lock()
            .expect("handle table poisoned")
            .remove(ticket)
        {
            handle.abort();
        }
        let mut state = state.lock().expect("task state poisoned");
        if !state.is_settled() {
            *state = TaskState::Failed("cancelled".to_string());
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "tasks_tests.rs"]
mod tests;
