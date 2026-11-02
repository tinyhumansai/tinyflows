//! `dry_run_workflow`: execute a DRAFT against tinyflows MOCK capabilities (ungated, F7).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tinyflows::model::WorkflowGraph;

use super::ports::{BuilderGates, DraftStore, FlowStore, SandboxRunRequest};
use super::static_schema;
use tinyflows::authoring::validate_and_migrate_graph;
use tinytools::{PermissionLevel, Tool, ToolResult};

// ─────────────────────────────────────────────────────────────────────────────
// dry_run_workflow — execute a DRAFT against MOCK capabilities (ungated, F7)
// ─────────────────────────────────────────────────────────────────────────────

/// `dry_run_workflow`: resolve an unsaved graph, then delegate execution to the
/// host's `SandboxRunner` port. Production hosts route that port through the
/// compiled module's `Runs.SandboxRun`; the report is shared with every
/// sandbox entry point.
///
/// **No real side effects:** the compiled module uses deterministic mocks for
/// LLM, agent, HTTP, code, shell, memory, state and approval capabilities. The
/// only callback it accepts is the host-bound `ToolInvoke` preflight; its
/// return value is ignored and the module supplies its own mock tool echo.
///
/// **Not autonomy-tier gated (F7):** `permission_level()` returns
/// [`PermissionLevel::None`], so a read-only agent may self-verify its proposal.
///
/// **Wiring preflight:** the host callback validates required tool arguments
/// before the module produces its deterministic mock echo. Missing or null
/// arguments therefore fail simulation with the same actionable diagnostic as
/// a real run.
///
/// **Null-resolution check (the "produces functionally-broken workflows" fix):**
/// a required arg can be present *and non-Composio* (a native `oh:` tool, or a
/// Composio arg the catalog has no cached schema for) and still be wired to a
/// `=`-expression that silently resolves to `null` — the preflight above only
/// catches a *missing/null Composio-required* arg, so a graph like that used to
/// dry-run green and then do nothing at runtime. The run is driven through
/// [`tinyflows::engine::run_with_observer`] with a `CapturingObserver` that
/// records every node's [`ExecutionStep::diagnostics`](tinyflows::observability::ExecutionStep)
/// — the `=`-expressions the vendored engine itself traced as null-resolved
/// (see `tinyflows::expr::resolve_traced`). After the run settles, every
/// diagnostic on a **`tool_call` node's `args.*` location** is collected; any
/// hit fails the dry run with `ok: false` and the offending
/// `{ node_id, location, expression }` list, rather than reporting `ok: true`
/// for a graph that would silently no-op. Diagnostics on any OTHER
/// `agent`-node config subfield are NOT fatal here — a null there degrades
/// output quality but doesn't break execution the way a null tool arg does.
///
/// **Agent-prompt null check:** the ONE `agent`-node diagnostic that IS fatal
/// is a null-resolved **`prompt` itself** (`location == "prompt"`) — `prompt`
/// is the node's only input channel to the completion, so a `null` there
/// means the agent runs with a completely EMPTY prompt (the root-cause bug
/// `config.input_context` and the host's `validate_binding_resolvability` static
/// gate both exist to prevent). Collected separately into
/// `agent_prompt_nulls` (`{ node_id, location, expression, suggestion }`) and
/// added to the same `ok: false` condition as `null_resolutions`.
///
/// **Agent-`input_context` null check:** the SAME treatment applies to a
/// null-resolved **`input_context`** (`location == "input_context"`) — since
/// #4590 this is the agent's primary upstream-data channel (the very field
/// `prompt`-embedded jq expressions were supposed to stop needing), so a
/// `null` here is just as execution-breaking as a null `prompt`: the agent
/// runs with no upstream data at all. Collected separately into
/// `agent_input_context_nulls` (`{ node_id, location, expression, suggestion }`,
/// mirroring `agent_prompt_nulls` exactly) and added to the same `ok: false`
/// condition as `null_resolutions`/`agent_prompt_nulls`.
///
/// **`on_error: continue`/`route` does not mask a `tool_call` failure either.**
/// Those policies convert an executor error (e.g. the required-arg preflight
/// rejecting a null arg) into a routed error ITEM so the *run* still completes
/// (`Ok(outcome)`) — the failing node's `ExecutionStep` carries an EMPTY
/// `diagnostics` (the null check above would miss it) but its `status` is
/// [`StepStatus::Error`](tinyflows::observability::StepStatus::Error). Every
/// such `tool_call` step is collected into `node_errors`
/// (`{ node_id, error }`, the error text read back out of the run's `output`
/// state — see `authoring_report::AuthoringReport`) and fails the dry run the same as
/// a null resolution.
///
/// **Routing-divergence warning (B15's dry-run blind spot):** none of the
/// checks above see a node that never ran at all. An `agent`/`tool_call` node
/// downstream of a `condition` can be silently unexercised because the
/// sandbox's mock trigger payload has a different *shape* than a real
/// trigger's (e.g. a webhook's real JSON body vs. the dry run's `{}`
/// default), so the condition takes a different branch under mock data than
/// it would at runtime — a graph can dry-run `ok: true` while its most
/// data-dependent node was never actually checked. After the run settles,
/// every `agent`/`tool_call` node with no `ExecutionStep` in the
/// `CapturingObserver` is collected into `routing_divergence_warnings`
/// (`{ node_id, condition_node_id, message }`, `condition_node_id` naming the
/// nearest upstream `condition` node found by walking predecessors — see
/// `tinyflows::diagnostics::nearest_upstream_condition` — or `null` if none is found). This is a
/// **warning, not a hard reject**: it never flips `ok` to `false` by itself
/// (an unexercised branch can be entirely intentional), and is surfaced on
/// both the `ok: true` and `ok: false` result shapes so the caller can
/// double-check that node's wiring by hand.
pub struct DryRunWorkflowTool {
    drafts: Arc<dyn DraftStore>,
    flows: Arc<dyn FlowStore>,
    gates: Arc<dyn BuilderGates>,
}

impl DryRunWorkflowTool {
    pub fn new(
        drafts: Arc<dyn DraftStore>,
        flows: Arc<dyn FlowStore>,
        gates: Arc<dyn BuilderGates>,
    ) -> Self {
        Self {
            drafts,
            flows,
            gates,
        }
    }
}

#[async_trait]
impl Tool for DryRunWorkflowTool {
    fn name(&self) -> &str {
        "dry_run_workflow"
    }

    fn description(&self) -> &str {
        "Dry-run a workflow graph in a SANDBOX to self-verify it before \
         proposing. Compiles the graph and executes it against MOCK capabilities \
         — every LLM / tool_call / http_request / code node returns a deterministic \
         echo, so NOTHING real happens (no messages sent, no code run). Returns the \
         simulated per-node output labeled as sandbox output. Use it to catch \
         wiring/routing mistakes; it does NOT prove real integrations work. Provide \
         the graph as exactly one of `draft_id` (a working draft), `flow_id` (a saved \
         flow), or inline `graph` (draft_id wins, then flow_id), plus an optional \
         `input`."
    }

    fn parameters_schema(&self) -> Value {
        static_schema!(include_str!("parameters/dry_run_workflow.json"))
    }

    fn permission_level(&self) -> PermissionLevel {
        // Mock-only and side-effect-free: nothing external ever fires (all
        // capabilities are echo stubs). So it needs no elevated permission and
        // is available on EVERY tier, read-only included (audit F7) — a
        // read-only agent must be able to self-verify its own proposal.
        PermissionLevel::None
    }

    fn external_effect(&self) -> bool {
        // Mock capabilities only — no real outbound effect.
        false
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        // Graph source: exactly one of a working draft, a saved flow, or an
        // inline graph — same precedence (draft_id > flow_id > graph) as the
        // sibling validate/edit tools, so they all accept the same handles.
        let draft_id = args
            .get("draft_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let flow_id = args
            .get("flow_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let inline_graph = args.get("graph").filter(|v| !v.is_null());

        let graph_json = match (draft_id, flow_id, inline_graph) {
            (Some(id), _, _) => match self.drafts.get(id).await {
                Ok(draft) => draft.graph,
                Err(e) => {
                    return Ok(ToolResult::error(format!(
                        "Could not load draft '{id}' to dry-run: {e}"
                    )));
                }
            },
            (None, Some(id), _) => match self.flows.load_graph(id).await {
                Ok(Some(graph)) => serde_json::to_value(&graph)?,
                Ok(None) => {
                    return Ok(ToolResult::error(format!("flow '{id}' not found")));
                }
                Err(e) => {
                    return Ok(ToolResult::error(format!(
                        "Could not load flow '{id}' to dry-run: {e}"
                    )));
                }
            },
            (None, None, Some(v)) => v.clone(),
            (None, None, None) => {
                return Ok(ToolResult::error(
                    "Provide one of `draft_id` (a working draft), `flow_id` (a saved flow), or \
                     `graph` (an inline graph) to dry-run."
                        .to_string(),
                ));
            }
        };
        let input = args.get("input").cloned().unwrap_or_else(|| json!({}));

        let graph: WorkflowGraph = match validate_and_migrate_graph(graph_json) {
            Ok(graph) => graph,
            Err(e) => {
                return Ok(ToolResult::error(format!(
                    "Cannot dry-run an invalid graph: {e}. Fix the graph first."
                )));
            }
        };

        tracing::debug!(
            target: "flows",
            node_count = graph.nodes.len(),
            "[flows] dry_run_workflow: delegating draft to compiled module sandbox"
        );
        let permissions_manifest = self.gates.approval_manifest(&graph).await;
        let result = self
            .gates
            .run(SandboxRunRequest {
                graph: serde_json::to_value(&graph)?,
                input,
                permissions_manifest,
            })
            .await;
        match result {
            Ok(report) => Ok(ToolResult::success(serde_json::to_string_pretty(&report)?)),
            Err(error) => Ok(ToolResult::error(error)),
        }
    }
}

#[cfg(test)]
#[path = "dry_run_schema_tests.rs"]
mod schema_tests;
