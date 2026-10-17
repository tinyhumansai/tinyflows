//! Wire contracts for workflow run control and host-provided capabilities.
//!
//! These records are serde-only. A `CapsHost` address is supplied by the
//! trusted host for one run; the module uses that exact address for every
//! callback and never chooses an ambient default from graph or trigger data.

use crate::catalog::CatalogStorage;
use serde::{Deserialize, Serialize};
use serde_json::Value;

mod caps_agent_types;
mod caps_types;

pub use caps_agent_types::*;
pub use caps_types::*;

/// Interface for run control on the shared module bus name.
pub const INTERFACE: &str = "ai.tinyhumans.tinyflows.Runs";
/// Run-control object path.
pub const OBJECT_PATH: &str = "/ai/tinyhumans/tinyflows/Runs";
/// Version of the run-control contract.
pub const CONTRACT_VERSION: u32 = 5;
/// Run-control members, in stable interface order.
pub const METHODS: &[&str] = &[
    "StartRun",
    "ResumeRun",
    "ExpireParked",
    "BootSweep",
    "CancelRun",
    "PollRun",
    "SandboxRun",
];

/// Stable member names.
pub mod methods {
    /// Start a run of a graph; returns a run id immediately.
    pub const START_RUN: &str = "StartRun";
    /// Resume a parked run with approvals or inputs.
    pub const RESUME_RUN: &str = "ResumeRun";
    /// Expire parked runs past their approval TTL.
    pub const EXPIRE_PARKED: &str = "ExpireParked";
    /// Reconcile orphaned local runs after module startup.
    pub const BOOT_SWEEP: &str = "BootSweep";
    /// Cooperatively cancel a run.
    pub const CANCEL_RUN: &str = "CancelRun";
    /// Read run events after a cursor.
    pub const POLL_RUN: &str = "PollRun";
    /// Execute an unsaved graph against deterministic sandbox capabilities.
    pub const SANDBOX_RUN: &str = "SandboxRun";
}

/// Interface the host serves so the module can reach every host capability.
pub const CAPS_INTERFACE: &str = "ai.tinyhumans.tinyflows.CapsHost";
/// Version of the host-served capability callback contract.
pub const CAPS_CONTRACT_VERSION: u32 = 1;
/// Maximum progress records accepted in one `AgentEvents` page.
pub const MAX_AGENT_EVENTS_PAGE: usize = 256;
/// Maximum progress records folded into one agent transcript.
pub const MAX_AGENT_TRANSCRIPT_EVENTS: usize = 10_000;

/// Host callback members, in stable interface order.
pub const CAPS_METHODS: &[&str] = &[
    caps::LLM_COMPLETE,
    caps::TOOL_INVOKE,
    caps::HTTP_REQUEST,
    caps::CODE_RUN,
    caps::SHELL_RUN,
    caps::MEMORY_RECALL,
    caps::MEMORY_FLAVOUR,
    caps::MEMORY_PEOPLE,
    caps::MEMORY_REMEMBER,
    caps::MEMORY_FORGET,
    caps::AGENT_RUN,
    caps::AGENT_EVENTS,
    caps::RESOLVE_AGENT,
    caps::LIST_AGENTS,
    caps::RESOLVE_CONTEXT,
    caps::RESOLVE_TOOLS,
    caps::RESOLVE_WORKDIR,
    caps::APPROVAL_DECIDE,
    caps::APPROVAL_CANCEL,
];

/// `StartRun` request. The host-supplied context, capabilities and storage
/// callback are a trusted execution envelope separate from graph JSON.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct StartRun {
    /// Host-chosen stable id. The exact opaque value is used for the row and
    /// graph checkpoint thread; empty or whitespace-only ids are rejected.
    pub run_id: String,
    /// Trusted flow identity and trace origin. Never derived from graph JSON.
    pub context: RunContext,
    /// Host callback already bound to this run's flow, owner and approval rule.
    pub caps_host: CapsHost,
    /// Host callback already bound to the acting owner's storage scope. Its
    /// exact canonical address is the owner key used for live Poll/Cancel.
    pub storage: CatalogStorage,
    /// Migrated graph JSON.
    pub graph: Value,
    /// Host trigger kind, separate from its free-form payload and inputs.
    pub trigger_kind: FlowRunTrigger,
    /// Payload delivered by the trigger node and exposed as `run.trigger`.
    pub trigger_data: Value,
    /// Values for declared workflow inputs.
    pub inputs: serde_json::Map<String, Value>,
    /// Required host-selected execution budget. Zero is invalid; the module
    /// enforces the deadline and cooperatively stops scheduling work at expiry.
    pub timeout_ms: u64,
}

/// Trusted run identity and host tracing metadata, kept out of graph/trigger data.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RunContext {
    /// Saved workflow identity used by scoped state and run history.
    pub flow_id: String,
    /// Host origin metadata, if present (for example a schedule or app event id).
    pub origin: Option<Value>,
}

/// Kind of host event that initiated this execution.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FlowRunTrigger {
    /// Explicit RPC/manual invocation.
    Rpc,
    /// Scheduler dispatch.
    Schedule,
    /// Application event dispatch.
    AppEvent,
    /// Human-approved continuation of a parked run (ResumeRun is served by a later slice).
    Resume,
}

/// Owner-scoped operation on an existing live run.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RunControlRequest {
    /// Stable owner-scoped document callback. It must match the binding used by
    /// StartRun; the module does not accept an owner id from model data.
    pub storage: CatalogStorage,
    /// Exact opaque run id supplied to StartRun.
    pub run_id: String,
    /// Last accepted event sequence for PollRun. Ignored by CancelRun.
    #[serde(default)]
    pub cursor: u64,
}

/// Immediate acknowledgement that a run was admitted and its actor registered.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct StartRunResponse {
    /// Exact host-chosen id used for this run.
    pub run_id: String,
    /// True when the exact owner-scoped id already has a live or parked run;
    /// false when this request created and started a new run. Existing
    /// terminal rows are rejected and never overwritten.
    pub already_running: bool,
}

/// Acknowledgement of a cooperative cancellation request.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct CancelRunResponse {
    /// True only when this request signalled a currently live run.
    pub signalled: bool,
}

/// `ResumeRun` request.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ResumeRun {
    /// Parked run.
    pub run_id: String,
    /// Expected owning workflow; the stored row and snapshot must agree.
    pub flow_id: String,
    /// Owner-scoped persisted documents and checkpoint storage.
    pub storage: CatalogStorage,
    /// Fresh host callback bound to this flow, run, owner and approval policy.
    pub caps_host: CapsHost,
    /// Approved node ids.
    pub approvals: Vec<String>,
    /// Rejected node ids.
    pub rejections: Vec<String>,
    /// Host-selected execution budget for the resumed segment.
    pub timeout_ms: u64,
}

/// Result of successfully admitting a parked run for resume.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ResumeRunResponse {
    /// Exact run id continued by the actor.
    pub run_id: String,
}

/// Owner-scoped parked-run expiry request.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ExpireParked {
    /// Owner-scoped catalog and checkpoint storage.
    pub storage: CatalogStorage,
}

/// Runs settled by a parked-run expiry call.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ExpireParkedResponse {
    /// Run ids and owning flow ids actually changed by this call.
    pub expired: Vec<(String, String)>,
}

/// Owner-scoped startup orphan-reconciliation request.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct BootSweep {
    /// Owner-scoped catalog and checkpoint storage.
    pub storage: CatalogStorage,
    /// Shared backends can contain actors from other replicas and must not be swept.
    pub shared_backend: bool,
}

/// Run ids this call actually changed to `interrupted`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct BootSweepResponse {
    /// Reconciled run ids.
    pub interrupted: Vec<String>,
}

/// One run event returned by `PollRun`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct RunEvent {
    /// Monotonic position; pass the last seen value as the next cursor.
    pub seq: u64,
    /// Event kind (`step`, `parked`, `finished`, `agent_progress`).
    pub kind: String,
    /// Event payload.
    pub data: Value,
    /// Graph runtime id, distinct from the durable flow run id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graph_run_id: Option<String>,
}

/// Authoritative terminal payload carried in a `finished` event's `data`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RunFinished {
    /// Terminal run status, preserved for display and forward compatibility.
    pub status: String,
    /// Whether the module classified the terminal status as successful.
    #[serde(default)]
    pub successful: bool,
    /// Failure or interruption detail, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Graph runtime id, distinct from the durable flow run id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graph_run_id: Option<String>,
}

/// `PollRun` response.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct PollRun {
    /// Events after the requested cursor.
    pub events: Vec<RunEvent>,
    /// Highest event sequence returned (or the input cursor when empty).
    pub next_cursor: u64,
    /// Whether the run reached a terminal status.
    pub done: bool,
}

/// Execute a graph without live run admission, persistence, or host effects.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct SandboxRun {
    /// Inline, draft, or otherwise unsaved graph JSON.
    pub graph: Value,
    /// Initial graph input; omitted callers should send an empty object.
    pub input: Value,
    /// Exact host callback address that performs ToolInvoke preflight only.
    pub tool_invoker: CallbackAddress,
    /// Permission report precomputed by the trusted host's policy classifier.
    /// It is included in the settled report and never authorizes execution.
    #[serde(default)]
    pub permissions_manifest: Vec<Value>,
}

/// Shared dry-run report envelope returned by `SandboxRun`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct SandboxRunResponse {
    /// Existing builder dry-run JSON shape, including `sandbox`, `ok`, output,
    /// diagnostics, advice and permission-manifest fields.
    pub result: Value,
}

/// Exact address of a callback object created by the trusted host.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct CallbackAddress {
    /// Bus destination of the callback object.
    pub destination: String,
    /// Callback object path.
    pub object_path: String,
}

/// Address to use for one run's capability calls. The host binds this address
/// to the flow, run, approval policy and owner scope before passing it to the
/// module. None of those values are accepted from the graph or trigger.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct CapsHost {
    /// Exact object address chosen by the trusted host for this run.
    pub callback: CallbackAddress,
}

/// Stable `CapsHost` member names and record conventions.
pub mod caps {
    /// `LlmProvider::complete(request, conn) -> Value`.
    pub const LLM_COMPLETE: &str = "LlmComplete";
    /// `ToolInvoker::invoke(slug, args, conn) -> Value`.
    pub const TOOL_INVOKE: &str = "ToolInvoke";
    /// `HttpClient::request(request, conn) -> Value`.
    pub const HTTP_REQUEST: &str = "HttpRequest";
    /// `CodeRunner::run(CodeRunRequest) -> Value`.
    pub const CODE_RUN: &str = "CodeRun";
    /// `ShellRunner::run(request) -> ShellOutcome`.
    pub const SHELL_RUN: &str = "ShellRun";
    /// Memory recall method.
    pub const MEMORY_RECALL: &str = "MemoryRecall";
    /// Memory flavour method.
    pub const MEMORY_FLAVOUR: &str = "MemoryFlavour";
    /// Memory people method.
    pub const MEMORY_PEOPLE: &str = "MemoryPeople";
    /// Memory remember method.
    pub const MEMORY_REMEMBER: &str = "MemoryRemember";
    /// Memory forget method.
    pub const MEMORY_FORGET: &str = "MemoryForget";
    /// Start a host-owned agent turn; returns an opaque event handle.
    pub const AGENT_RUN: &str = "AgentRun";
    /// Read agent events after a cursor.
    pub const AGENT_EVENTS: &str = "AgentEvents";
    /// Resolve a host agent definition; `None` means ordinary absence.
    pub const RESOLVE_AGENT: &str = "ResolveAgent";
    /// List host agent definitions.
    pub const LIST_AGENTS: &str = "ListAgents";
    /// Resolve host context; `None` means ordinary absence.
    pub const RESOLVE_CONTEXT: &str = "ResolveContext";
    /// Expand allowed tool grants into descriptors.
    pub const RESOLVE_TOOLS: &str = "ResolveTools";
    /// Resolve an agent work directory.
    pub const RESOLVE_WORKDIR: &str = "ResolveWorkdir";
    /// Idempotently create or fetch an approval request.
    pub const APPROVAL_DECIDE: &str = "ApprovalDecide";
    /// Best-effort withdrawal of an approval request.
    pub const APPROVAL_CANCEL: &str = "ApprovalCancel";
}

#[cfg(test)]
#[path = "runs_tests.rs"]
mod tests;
