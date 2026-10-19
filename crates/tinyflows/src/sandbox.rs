//! Deterministic execution used by builder dry-runs and the module sandbox.
//!
//! The caller supplies the only capability that may leave the module (tool
//! invocation); every report and failure classification is shared here.

use std::time::Duration;

use serde_json::Value;
use serde_json::json;

use crate::authoring_report::{self, AuthoringReport};
use crate::caps::Capabilities;
use crate::model::WorkflowGraph;
use crate::observability::CapturingObserver;

/// The settled result of a mock-only graph execution.
#[derive(Debug)]
pub struct SandboxOutcome {
    /// Final merged state from the graph.
    pub output: Value,
    /// Approval requests left by the mock approval surface.
    pub pending_approvals: Vec<String>,
    /// Diagnostics shared by every sandbox entry point.
    pub report: AuthoringReport,
}

/// Why a sandbox execution did not settle.
#[derive(Debug)]
pub enum SandboxError {
    /// Execution exceeded its caller-selected deadline.
    TimedOut,
    /// Engine execution failed, with bindings the echo tool cannot verify.
    Failed {
        /// Engine error text.
        message: String,
        /// Required args bound to an upstream tool's real output.
        unverifiable_bindings: Vec<authoring_report::NullResolutionEntry>,
    },
}

/// Executes a compiled graph and builds the canonical authoring report.
///
/// Compile and validate before calling. The `Capabilities` passed here should
/// contain deterministic mocks for every slot except an optional host-bound
/// tool preflight callback. No engine state is persisted by this function.
pub async fn execute(
    graph: &WorkflowGraph,
    compiled: &crate::compiler::CompiledWorkflow,
    input: Value,
    capabilities: &Capabilities,
    timeout: Duration,
) -> Result<SandboxOutcome, SandboxError> {
    let observer = std::sync::Arc::new(CapturingObserver::default());
    let observer_dyn: std::sync::Arc<dyn crate::observability::RunObserver> = observer.clone();
    let run = crate::engine::run_with_observer(compiled, input, capabilities, &observer_dyn);
    let outcome = match tokio::time::timeout(timeout, run).await {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(error)) => {
            return Err(SandboxError::Failed {
                message: error.to_string(),
                unverifiable_bindings: authoring_report::unverifiable_bindings(
                    graph,
                    &observer.steps(),
                ),
            });
        }
        Err(_) => return Err(SandboxError::TimedOut),
    };
    let report = AuthoringReport::build(graph, &observer.steps(), &outcome.output);
    Ok(SandboxOutcome {
        output: outcome.output,
        pending_approvals: outcome.pending_approvals,
        report,
    })
}

/// Converts an execution result to the canonical builder dry-run JSON report.
///
/// Compile and migration failures remain caller errors, as they are in the
/// copilot tool. Timed-out executions likewise remain tool errors; a settled
/// or failed node execution returns a sandbox report with `ok: false`.
pub fn report(
    execution: Result<SandboxOutcome, SandboxError>,
    timeout_secs: u64,
    permissions_manifest: Vec<Value>,
) -> Result<Value, String> {
    let outcome = match execution {
        Ok(outcome) => outcome,
        Err(SandboxError::TimedOut) => {
            return Err(format!("Sandbox dry-run timed out after {timeout_secs}s"));
        }
        Err(SandboxError::Failed {
            message,
            unverifiable_bindings,
        }) => {
            let result = if unverifiable_bindings.is_empty() {
                json!({
                    "sandbox": true,
                    "ok": false,
                    "error": message,
                    "note": "SANDBOX (mock) output — a node errored during simulation. No real side effects occurred."
                })
            } else {
                json!({
                    "sandbox": true,
                    "ok": false,
                    "error": message,
                    "unverifiable_bindings": unverifiable_bindings,
                    "note": "SANDBOX (mock) output — a tool_call node aborted because a required arg binds to the output of an upstream Composio tool_call, which the sandbox can only ECHO (it never produces real tool output fields). See unverifiable_bindings: each MAY already be wired correctly — confirm the path with get_tool_contract {{ slug }} (output_fields / primary_array_path; Composio results nest under .item.json.data.) or get_tool_output_sample {{ slug, args }} instead of re-wiring blindly. No real side effects occurred."
                })
            };
            return Ok(result);
        }
    };
    let report = outcome.report;
    let code_summary = code_summary(&report.unexecuted_code_nodes);
    let result = if report.is_failing() {
        json!({
            "sandbox": true,
            "ok": false,
            "null_resolutions": report.null_resolutions,
            "agent_prompt_nulls": report.agent_prompt_nulls,
            "agent_input_context_nulls": report.agent_input_context_nulls,
            "node_errors": report.node_errors,
            "routing_divergence_warnings": report.routing_divergence_warnings,
            "unexecuted_code_nodes": report.unexecuted_code_nodes,
            "code_source_errors": report.code_source_errors,
            "permissions_manifest": permissions_manifest,
            "message": format!("{code_summary}These tool_call args resolved to null, an agent node's prompt or input_context resolved to null (an EMPTY prompt — see agent_prompt_nulls — or no upstream data at all — see agent_input_context_nulls), or a tool_call node failed during the sandbox run (even one recovered via on_error: continue/route) — wire null-resolved args from an upstream node's real output (give any agent node an output_parser.schema so its fields are addressable), feed upstream data into a null-resolved agent prompt/input_context from a real upstream field instead of a jq expression inside the prompt text, and fix or rewire whatever tool_call node_errors names. Also check routing_divergence_warnings: any agent/tool_call node listed there never ran in this sandbox at all because an upstream condition routed the mock data past it — verify that wiring by hand too. A code_source_errors entry is a script that reads an undefined `item` and fails every real run: rewrite it to read `input`.")
        })
    } else {
        json!({
            "sandbox": true,
            "ok": true,
            "output": outcome.output,
            "pending_approvals": outcome.pending_approvals,
            "null_resolutions": report.null_resolutions,
            "agent_prompt_nulls": report.agent_prompt_nulls,
            "agent_input_context_nulls": report.agent_input_context_nulls,
            "node_errors": report.node_errors,
            "routing_divergence_warnings": report.routing_divergence_warnings,
            "unexecuted_code_nodes": report.unexecuted_code_nodes,
            "code_source_errors": report.code_source_errors,
            "permissions_manifest": permissions_manifest,
            "note": format!("{code_summary}SANDBOX (mock) output — LLM/tool/HTTP/code nodes returned deterministic echoes; NO real side effects occurred. This checks wiring/routing only, not whether real integrations work. If routing_divergence_warnings is non-empty, an agent/tool_call node never ran in this sandbox because an upstream condition routed the mock data past it — that node's wiring is unverified; check it by hand.")
        })
    };
    Ok(result)
}

/// The line that leads the report when the graph has `code` nodes, so a
/// builder cannot read a clean dry run as proof that their scripts work.
fn code_summary(nodes: &[authoring_report::UnexecutedCodeNode]) -> String {
    if nodes.is_empty() {
        return String::new();
    }
    let ids: Vec<&str> = nodes.iter().map(|n| n.node_id.as_str()).collect();
    format!(
        "CODE NOT EXECUTED: {} code node(s) ({}) were stubbed, not run, so their scripts are \
         UNVERIFIED by this dry run; do not report them as working (see unexecuted_code_nodes). ",
        ids.len(),
        ids.join(", ")
    )
}

#[cfg(test)]
#[path = "sandbox_tests.rs"]
mod tests;
