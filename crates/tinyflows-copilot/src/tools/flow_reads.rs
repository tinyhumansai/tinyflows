//! Read-only saved-flow tools: history, run listing, flow listing, one flow, one run.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::ports::FlowStore;
use tinytools::{PermissionLevel, Tool, ToolResult};

// ─────────────────────────────────────────────────────────────────────────────
// get_flow_history — read-only: prior graph snapshots (F6)
// ─────────────────────────────────────────────────────────────────────────────

/// `get_flow_history`: read a saved flow's revision history — the prior graph
/// snapshots captured on each update. Lets the agent see what changed and pick
/// a revision to roll back to (the user drives the actual rollback RPC).
pub struct GetFlowHistoryTool {
    flows: Arc<dyn FlowStore>,
}

impl GetFlowHistoryTool {
    pub fn new(flows: Arc<dyn FlowStore>) -> Self {
        Self { flows }
    }
}

#[async_trait]
impl Tool for GetFlowHistoryTool {
    fn name(&self) -> &str {
        "get_flow_history"
    }

    fn description(&self) -> &str {
        "List a saved flow's revision history — the prior graph snapshots captured automatically \
         on each update (newest first, capped). Read-only. Returns a JSON array of { id, flow_id, \
         graph, name, require_approval, created_at }. Use it to see what a flow looked like before \
         a change, or to find the revision id the user can roll back to."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "flow_id": { "type": "string", "description": "The saved flow whose history to list." },
                "limit": { "type": "integer", "description": "Max revisions to return (default 20)." }
            },
            "required": ["flow_id"],
            "additionalProperties": false
        })
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::None
    }

    fn external_effect(&self) -> bool {
        false
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let flow_id = match args.get("flow_id").and_then(Value::as_str).map(str::trim) {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => return Ok(ToolResult::error("Missing 'flow_id' parameter".to_string())),
        };
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .unwrap_or(20);
        tracing::debug!(target: "flows", %flow_id, limit, "[flows] get_flow_history: listing revisions (read-only)");
        match self.flows.history(&flow_id, limit).await {
            Ok(revisions) => Ok(ToolResult::success(serde_json::to_string_pretty(
                &json!({ "revisions": revisions }),
            )?)),
            Err(e) => Ok(ToolResult::error(format!(
                "Could not load history for flow '{flow_id}': {e}"
            ))),
        }
    }
}

/// `list_flow_runs`: read-only listing of a saved flow's recent runs (id /
/// status / timestamps), so the agent can FIND a failing run to diagnose
/// instead of needing a run_id handed to it externally — the missing first step
/// of the self-debug loop (audit F4).
pub struct ListFlowRunsTool {
    flows: Arc<dyn FlowStore>,
}

impl ListFlowRunsTool {
    pub fn new(flows: Arc<dyn FlowStore>) -> Self {
        Self { flows }
    }
}

#[async_trait]
impl Tool for ListFlowRunsTool {
    fn name(&self) -> &str {
        "list_flow_runs"
    }

    fn description(&self) -> &str {
        "List a saved flow's recent runs (newest first) so you can find one to diagnose with \
         get_flow_run. Read-only. Returns a JSON array of runs { id, flow_id, thread_id, status, \
         started_at, finished_at?, error? }. `id`/`thread_id` is the run id you pass to \
         get_flow_run / resume_flow_run / cancel_flow_run."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "flow_id": { "type": "string", "description": "The saved flow whose runs to list." },
                "limit": { "type": "integer", "description": "Max runs to return (default 20)." }
            },
            "required": ["flow_id"],
            "additionalProperties": false
        })
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::None
    }

    fn external_effect(&self) -> bool {
        false
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let flow_id = match args.get("flow_id").and_then(Value::as_str).map(str::trim) {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => return Ok(ToolResult::error("Missing 'flow_id' parameter".to_string())),
        };
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .unwrap_or(20);
        tracing::debug!(target: "flows", %flow_id, limit, "[flows] list_flow_runs: listing runs (read-only)");
        match self.flows.list_runs(&flow_id, limit).await {
            Ok(runs) => Ok(ToolResult::success(serde_json::to_string_pretty(
                &json!({ "runs": runs }),
            )?)),
            Err(e) => Ok(ToolResult::error(format!(
                "Could not list runs for flow '{flow_id}': {e}"
            ))),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// list_flows — read-only: saved flow summaries
// ─────────────────────────────────────────────────────────────────────────────

/// `list_flows`: read-only listing of saved flows (id / name / enabled /
/// last_status) so the builder can reference, clone, or avoid duplicating an
/// existing automation.
pub struct ListFlowsTool {
    flows: Arc<dyn FlowStore>,
}

impl ListFlowsTool {
    pub fn new(flows: Arc<dyn FlowStore>) -> Self {
        Self { flows }
    }
}

#[async_trait]
impl Tool for ListFlowsTool {
    fn name(&self) -> &str {
        "list_flows"
    }

    fn description(&self) -> &str {
        "List the user's saved automation flows (tinyflows workflows). Read-only. \
         Returns a JSON array of { id, name, enabled, last_status, last_run_at } so \
         you can reference an existing flow, clone its structure (fetch the full \
         graph with get_flow), or avoid proposing a duplicate."
    }

    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {}, "additionalProperties": false })
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::None
    }

    fn external_effect(&self) -> bool {
        false
    }

    async fn execute(&self, _args: Value) -> anyhow::Result<ToolResult> {
        tracing::debug!(target: "flows", "[flows] list_flows: listing saved flows (read-only)");
        match self.flows.list().await {
            Ok(listed) => {
                let flows: Vec<Value> = listed
                    .iter()
                    .map(|f| {
                        json!({
                            "id": f.id,
                            "name": f.name,
                            "enabled": f.enabled,
                            "last_status": f.last_status,
                            "last_run_at": f.last_run_at,
                        })
                    })
                    .collect();
                Ok(ToolResult::success(serde_json::to_string_pretty(
                    &json!({ "flows": flows }),
                )?))
            }
            Err(e) => Ok(ToolResult::error(format!("Failed to list flows: {e}"))),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// get_flow — read-only: a saved flow's graph
// ─────────────────────────────────────────────────────────────────────────────

/// `get_flow`: read-only fetch of a saved flow's full `WorkflowGraph` by id,
/// so the builder can clone or extend an existing automation.
pub struct GetFlowTool {
    flows: Arc<dyn FlowStore>,
}

impl GetFlowTool {
    pub fn new(flows: Arc<dyn FlowStore>) -> Self {
        Self { flows }
    }
}

#[async_trait]
impl Tool for GetFlowTool {
    fn name(&self) -> &str {
        "get_flow"
    }

    fn description(&self) -> &str {
        "Fetch a saved flow's full tinyflows WorkflowGraph (nodes + edges) plus \
         its metadata by id. Read-only. Use it to clone or extend an existing \
         automation — pass the returned graph (possibly modified) to \
         revise_workflow or dry_run_workflow."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "The saved flow's id (from list_flows)." }
            },
            "required": ["id"],
            "additionalProperties": false
        })
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::None
    }

    fn external_effect(&self) -> bool {
        false
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let id = match args.get("id").and_then(Value::as_str).map(str::trim) {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => return Ok(ToolResult::error("Missing 'id' parameter".to_string())),
        };
        tracing::debug!(target: "flows", flow_id = %id, "[flows] get_flow: fetching saved flow (read-only)");
        match self.flows.get(&id).await {
            Ok(f) => {
                let graph = serde_json::to_value(&f.graph)?;
                Ok(ToolResult::success(serde_json::to_string_pretty(&json!({
                    "id": f.id,
                    "name": f.name,
                    "enabled": f.enabled,
                    "require_approval": f.require_approval,
                    "last_status": f.last_status,
                    "graph": graph,
                }))?))
            }
            Err(e) => Ok(ToolResult::error(format!("Failed to get flow '{id}': {e}"))),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// get_flow_run — read-only: a run's steps (for repair/debugging)
// ─────────────────────────────────────────────────────────────────────────────

/// `get_flow_run`: read-only fetch of a single flow run's step records, so the
/// builder can diagnose a failure and propose a repair.
pub struct GetFlowRunTool {
    flows: Arc<dyn FlowStore>,
}

impl GetFlowRunTool {
    pub fn new(flows: Arc<dyn FlowStore>) -> Self {
        Self { flows }
    }
}

#[async_trait]
impl Tool for GetFlowRunTool {
    fn name(&self) -> &str {
        "get_flow_run"
    }

    fn description(&self) -> &str {
        "Fetch a single flow run's record by run id: status, per-node step \
         results, any pending approvals, and the error (if it failed). Read-only. \
         Use it to debug a failing flow from an error report and propose a repair."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "run_id": { "type": "string", "description": "The run id (also the run's thread_id)." }
            },
            "required": ["run_id"],
            "additionalProperties": false
        })
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::None
    }

    fn external_effect(&self) -> bool {
        false
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let run_id = match args.get("run_id").and_then(Value::as_str).map(str::trim) {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => return Ok(ToolResult::error("Missing 'run_id' parameter".to_string())),
        };
        tracing::debug!(target: "flows", %run_id, "[flows] get_flow_run: fetching run record (read-only)");
        match self.flows.get_run(&run_id).await {
            Ok(run) => Ok(ToolResult::success(serde_json::to_string_pretty(&run)?)),
            Err(e) => Ok(ToolResult::error(format!(
                "Failed to get flow run '{run_id}': {e}"
            ))),
        }
    }
}
