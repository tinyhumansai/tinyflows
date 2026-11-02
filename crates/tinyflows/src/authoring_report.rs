//! The authoring-time dry-run report a builder host shows an agent.
//!
//! [`diagnostics::diagnose`] reads a simulation
//! generically. A *builder* surface wants the same evidence bucketed by what the
//! author should do about it, with advice worded for the kinds a host actually
//! ships: null `tool_call` args (with the honest "unverifiable" variant when the
//! binding reads from an upstream tool whose real output a sandbox can only
//! echo), null agent `prompt` / `input_context`, `tool_call` errors hidden by
//! `on_error: continue|route`, and nodes a condition routed the sample past.
//!
//! Pure over `(graph, steps, run output)`. The host keeps the mock
//! capabilities, the wall-clock timeout, the approval manifest and the JSON
//! envelope around this; the serialized shape of each entry is a wire contract
//! for the builder tool and is pinned by `authoring_report_tests`.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::diagnostics::{nearest_upstream_condition, node_error_message};
use crate::expr::NullResolution;
use crate::model::{Node, NodeKind, WorkflowGraph};
use crate::observability::{ExecutionStep, StepStatus};
use crate::preflight::mock_opaque_tool_call_upstream_ref;

/// One null-resolved `args.*` expression on a `tool_call` node.
///
/// Field order is the serialized order. The `unverifiable` variant carries the
/// three trailing fields; the plain one omits them.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NullResolutionEntry {
    /// The `tool_call` node whose config held it.
    pub node_id: String,
    /// The config location, e.g. `args.to`.
    pub location: String,
    /// The expression as written.
    pub expression: String,
    /// `Some(true)` when the expression binds to the output of an upstream
    /// `tool_call` a sandbox can only echo, so the null proves nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unverifiable: Option<bool>,
    /// The upstream `tool_call` node it binds to (unverifiable variant only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_tool_call: Option<String>,
    /// What to do about it (unverifiable variant only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<String>,
}

impl NullResolutionEntry {
    /// Whether this is the honest "the sandbox cannot settle this" variant.
    pub fn is_unverifiable(&self) -> bool {
        self.unverifiable == Some(true)
    }
}

/// A null-resolved agent `prompt` or `input_context`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentNullEntry {
    /// The agent node.
    pub node_id: String,
    /// `prompt` or `input_context`.
    pub location: String,
    /// The expression as written.
    pub expression: String,
    /// What to do about it.
    pub suggestion: String,
}

/// A `tool_call` node that failed in the sandbox, even if `on_error` recovered it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NodeErrorEntry {
    /// The failed node.
    pub node_id: String,
    /// The engine's message, or a generic one when none survived.
    pub error: String,
}

/// An agent / `tool_call` node the sample never reached.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RoutingDivergenceWarning {
    /// The node that did not execute.
    pub node_id: String,
    /// The nearest upstream `condition`, when one exists.
    pub condition_node_id: Option<String>,
    /// Human-readable explanation.
    pub message: String,
}

/// A `code` node the sandbox stubbed instead of running its `source`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UnexecutedCodeNode {
    /// The `code` node.
    pub node_id: String,
    /// Human-readable explanation.
    pub message: String,
}

/// A `code` node whose `source` reads an `item` variable it never defines.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CodeSourceError {
    /// The `code` node.
    pub node_id: String,
    /// What fails at run time, and what to read instead.
    pub message: String,
}

/// Every bucket of the authoring report.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuthoringReport {
    /// Null `args.*` on `tool_call` nodes.
    pub null_resolutions: Vec<NullResolutionEntry>,
    /// Null agent `prompt`s (an empty instruction).
    pub agent_prompt_nulls: Vec<AgentNullEntry>,
    /// Null agent `input_context`s (no upstream data).
    pub agent_input_context_nulls: Vec<AgentNullEntry>,
    /// `tool_call` failures, including ones `on_error` swallowed.
    pub node_errors: Vec<NodeErrorEntry>,
    /// Agent / `tool_call` nodes the sample routed past.
    pub routing_divergence_warnings: Vec<RoutingDivergenceWarning>,
    /// Every `code` node: the sandbox never runs their scripts.
    #[serde(default)]
    pub unexecuted_code_nodes: Vec<UnexecutedCodeNode>,
    /// `code` sources that appear to read an undefined `item` (a token-scan
    /// heuristic, advisory).
    #[serde(default)]
    pub code_source_errors: Vec<CodeSourceError>,
}

impl AuthoringReport {
    /// Whether anything here should fail the dry run. Routing warnings,
    /// unexecuted `code` nodes and code-source hints are advisory and never do.
    pub fn is_failing(&self) -> bool {
        !self.null_resolutions.is_empty()
            || !self.agent_prompt_nulls.is_empty()
            || !self.agent_input_context_nulls.is_empty()
            || !self.node_errors.is_empty()
    }

    /// Reads a settled run: `steps` as the engine reported them and `output`,
    /// its final merged state.
    pub fn build(graph: &WorkflowGraph, steps: &[ExecutionStep], output: &Value) -> Self {
        let ids = |kind: NodeKind| -> HashSet<&str> {
            graph
                .nodes
                .iter()
                .filter(|n| n.kind == kind)
                .map(|n| n.id.as_str())
                .collect()
        };
        let tool_call_ids = ids(NodeKind::ToolCall);
        let agent_ids = ids(NodeKind::Agent);

        let node_errors = steps
            .iter()
            .filter(|s| {
                tool_call_ids.contains(s.node_id.as_str()) && matches!(s.status, StepStatus::Error)
            })
            .map(|step| NodeErrorEntry {
                node_id: step.node_id.clone(),
                error: node_error_message(output, &step.node_id).unwrap_or_else(|| {
                    format!(
                        "tool_call node '{}' failed during the sandbox run — its `on_error` \
                         policy turned the failure into routed/continued data instead of \
                         failing the whole dry run, but the underlying error still means the \
                         node is broken.",
                        step.node_id
                    )
                }),
            })
            .collect();

        let executed: HashSet<&str> = steps.iter().map(|s| s.node_id.as_str()).collect();
        let routing_divergence_warnings = graph
            .nodes
            .iter()
            .filter(|n| {
                n.kind != NodeKind::Trigger
                    && (agent_ids.contains(n.id.as_str()) || tool_call_ids.contains(n.id.as_str()))
                    && !executed.contains(n.id.as_str())
            })
            .map(|node| {
                let condition_node_id = nearest_upstream_condition(graph, &node.id);
                let message = match &condition_node_id {
                    Some(cid) => format!(
                        "Node '{}' did not execute in the dry run (condition '{}' routed to \
                         the other branch under mock data); verify the wiring — at runtime \
                         with real data it may route differently.",
                        node.id, cid
                    ),
                    None => format!(
                        "Node '{}' did not execute in the dry run (an upstream branch routed \
                         the mock data away from it); verify the wiring — at runtime with real \
                         data it may route differently.",
                        node.id
                    ),
                };
                RoutingDivergenceWarning {
                    node_id: node.id.clone(),
                    condition_node_id,
                    message,
                }
            })
            .collect();

        Self {
            null_resolutions: tool_call_arg_nulls(graph, steps),
            agent_prompt_nulls: agent_nulls(
                steps,
                &agent_ids,
                "prompt",
                "Feed upstream data via input_context:\"=item\" and \
                                make the prompt a plain instruction.",
            ),
            agent_input_context_nulls: agent_nulls(
                steps,
                &agent_ids,
                "input_context",
                "Wire input_context from a real upstream field, e.g. \
                                \"=nodes.<node_id>.item.json.<field>\" (or \"=item\" off the \
                                trigger), not an expression that resolves to null.",
            ),
            node_errors,
            routing_divergence_warnings,
            unexecuted_code_nodes: unexecuted_code_nodes(graph),
            code_source_errors: code_source_errors(graph),
        }
    }
}

fn code_nodes(graph: &WorkflowGraph) -> impl Iterator<Item = &Node> {
    graph.nodes.iter().filter(|n| n.kind == NodeKind::Code)
}

/// One entry per `code` node in the graph, whether or not the sample reached
/// it: the sandbox runs none of them.
pub fn unexecuted_code_nodes(graph: &WorkflowGraph) -> Vec<UnexecutedCodeNode> {
    code_nodes(graph)
        .map(|node| UnexecutedCodeNode {
            node_id: node.id.clone(),
            message: format!(
                "code node '{}' was NOT executed: the sandbox stubs it to return \
                 {{ \"result\": <input> }}, so this dry run says nothing about whether its \
                 script works. Check its source against the code contract: a function body that \
                 reads `input` (the array of input items, payload under .json) and `return`s \
                 the output.",
                node.id
            ),
        })
        .collect()
}

/// `code` nodes whose source appears to read `item` without binding it.
pub fn code_source_errors(graph: &WorkflowGraph) -> Vec<CodeSourceError> {
    code_nodes(graph)
        .filter_map(|node| {
            let config = &node.config;
            let python = config.get("language").and_then(Value::as_str) == Some("python");
            let source = config.get("source").and_then(Value::as_str)?;
            code_source::reads_undefined_item(source, python).then(|| CodeSourceError {
                node_id: node.id.clone(),
                message: format!(
                    "code node '{}' appears to read a variable `item` its source never defines; \
                     if it does, every real run fails with {}. The script receives only `input`, \
                     the array of input items: read input[0].json for one item, or map over input \
                     for each.",
                    node.id,
                    if python {
                        "NameError"
                    } else {
                        "ReferenceError"
                    }
                ),
            })
        })
        .collect()
}

fn agent_nulls(
    steps: &[ExecutionStep],
    agent_ids: &HashSet<&str>,
    location: &str,
    suggestion: &str,
) -> Vec<AgentNullEntry> {
    steps
        .iter()
        .filter(|s| agent_ids.contains(s.node_id.as_str()))
        .flat_map(|s| {
            s.diagnostics
                .iter()
                .filter(|d| d.location == location)
                .map(|d| AgentNullEntry {
                    node_id: s.node_id.clone(),
                    location: d.location.clone(),
                    expression: d.expression.clone(),
                    suggestion: suggestion.to_string(),
                })
        })
        .collect()
}

/// Every null-resolved `args.*` expression on a `tool_call` node. Shared by the
/// settled-run report (which fails on these) and [`unverifiable_bindings`]
/// (the errored-run path).
pub fn tool_call_arg_nulls(
    graph: &WorkflowGraph,
    steps: &[ExecutionStep],
) -> Vec<NullResolutionEntry> {
    let tool_call_ids: HashSet<&str> = graph
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::ToolCall)
        .map(|n| n.id.as_str())
        .collect();
    steps
        .iter()
        .filter(|s| tool_call_ids.contains(s.node_id.as_str()))
        .flat_map(|s| {
            s.diagnostics
                .iter()
                .filter(|d| d.location == "args" || d.location.starts_with("args."))
                .map(|d| null_resolution_entry(&s.node_id, d, graph))
        })
        .collect()
}

/// Only the unverifiable entries: what an errored run surfaces so a
/// stop-policy preflight abort explains itself honestly instead of via the
/// generic required-arg text.
pub fn unverifiable_bindings(
    graph: &WorkflowGraph,
    steps: &[ExecutionStep],
) -> Vec<NullResolutionEntry> {
    tool_call_arg_nulls(graph, steps)
        .into_iter()
        .filter(NullResolutionEntry::is_unverifiable)
        .collect()
}

/// One entry, marked `unverifiable` when the null-resolved expression binds to
/// the output of an upstream `tool_call` (Composio or native) a sandbox can only
/// echo: that null is expected there and does not prove the binding wrong. The
/// advice adapts to the upstream kind — a Composio result nests under
/// `.item.json.data.`, a native `oh:` tool's output binds flat.
fn null_resolution_entry(
    node_id: &str,
    diag: &NullResolution,
    graph: &WorkflowGraph,
) -> NullResolutionEntry {
    let Some(upstream) = mock_opaque_tool_call_upstream_ref(&diag.expression, graph, node_id)
    else {
        return NullResolutionEntry {
            node_id: node_id.to_string(),
            location: diag.location.clone(),
            expression: diag.expression.clone(),
            unverifiable: None,
            upstream_tool_call: None,
            suggestion: None,
        };
    };
    let field = diag.location.strip_prefix("args.").unwrap_or("args");
    let upstream_is_native = graph
        .nodes
        .iter()
        .find(|n| n.id == upstream)
        .and_then(|n| n.config.get("slug").and_then(Value::as_str))
        .is_some_and(|s| s.starts_with("oh:"));
    let suggestion = if upstream_is_native {
        format!(
            "required arg `{field}` binds to the output of native tool_call node \
             `{upstream}` — the SANDBOX only echoes tool calls and can never produce \
             their real output fields, so this binding is UNVERIFIABLE here (not \
             necessarily wrong). A native `oh:` tool's real output binds FLAT at \
             `=nodes.{upstream}.item.json.<field>` (no `.data.` wrapper). Confirm the \
             field name against that tool's own output shape. It is a real bug only if \
             the path doesn't match the tool's actual output."
        )
    } else {
        format!(
            "required arg `{field}` binds to the output of Composio tool_call node \
             `{upstream}` — the SANDBOX only echoes tool calls and can never produce \
             their real output fields, so this binding is UNVERIFIABLE here (not \
             necessarily wrong). Confirm the path against get_tool_contract {{ slug }}'s \
             output_fields / primary_array_path (remember Composio results nest under \
             `.item.json.data.`), or get_tool_output_sample {{ slug, args }} for the \
             real shape. It is a real bug only if the path doesn't match the action's \
             actual output."
        )
    };
    NullResolutionEntry {
        node_id: node_id.to_string(),
        location: diag.location.clone(),
        expression: diag.expression.clone(),
        unverifiable: Some(true),
        upstream_tool_call: Some(upstream.to_string()),
        suggestion: Some(suggestion),
    }
}

#[path = "code_source.rs"]
mod code_source;

#[cfg(test)]
#[path = "authoring_report_tests.rs"]
mod tests;
