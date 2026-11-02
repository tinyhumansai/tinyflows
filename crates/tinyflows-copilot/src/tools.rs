//! The authoring tool belt for a `workflow-builder` agent.
//!
//! These tools give the `workflow-builder` agent its full authoring surface for
//! tinyflows `WorkflowGraph`s in chat — tools spanning propose-only
//! validation, in-place draft mutation, reads, run control, and persistence:
//!
//! | Tool                             | Permission | Effect                                                        |
//! | --------------------------------- | ---------- | ---------------------------------------------------------------- |
//! | [`ReviseWorkflowTool`]            | `None`     | validate a revised draft → proposal (never persists)              |
//! | [`EditWorkflowTool`]              | `None`     | mutate a draft in place (`add_node`/`remove_edge`/…) — never saves |
//! | [`ValidateWorkflowTool`]          | `None`     | read: run the full gate stack without emitting a proposal card    |
//! | [`GetFlowHistoryTool`]            | `None`     | read: a saved flow's prior graph snapshots                        |
//! | [`ListFlowRunsTool`]              | `None`     | read: a flow's run history                                        |
//! | [`ResumeFlowRunTool`]             | `Execute`  | advance a run parked on approval — approved nodes fire for real   |
//! | [`CancelFlowRunTool`]             | `Write`    | stop an in-flight/parked run; fires no new outbound effect        |
//! | [`CreateWorkflowTool`]            | `Write`    | persist a NEW flow — always born **DISABLED**                     |
//! | [`DuplicateFlowTool`]             | `Write`    | clone a saved flow — the copy is always born **DISABLED**         |
//! | [`ListFlowsTool`]                 | `None`     | read: list saved flows                                            |
//! | [`GetFlowTool`]                   | `None`     | read: fetch a saved flow's graph                                  |
//! | [`GetFlowRunTool`]                | `None`     | read: fetch a run's steps                                         |
//! | [`ListAgentDefinitionsTool`]      | `None`     | read: selectable agent definitions (`agent_ref`)                  |
//! | [`ListNodeKindsTool`]             | `None`     | read: the DSL's node kinds                                        |
//! | [`GetNodeKindContractTool`]       | `None`     | read: one node kind's config/port/example/gotcha contract         |
//! | [`DryRunWorkflowTool`]            | `None`     | run a *draft* against MOCK capabilities — not tier-gated (F7)     |
//! | [`SaveWorkflowTool`]              | `Write`    | persist a graph onto an EXISTING flow                              |
//! | [`SuggestWorkflowsTool`]          | `None`     | the discovery agent's terminal sink: record workflow pitches      |
//!
//! **Human-in-the-loop invariant.** Enabling a flow is not a tool this agent
//! has, by design, no matter which tool above it reaches for:
//! [`CreateWorkflowTool`] and [`DuplicateFlowTool`] always produce a
//! **DISABLED** flow, and [`SaveWorkflowTool`] never sets
//! `enabled`/`require_approval` (it CAN auto-disable an already-enabled flow
//! when the graph's trigger transitions from manual to automatic — see its
//! own doc). `revise_workflow` / `edit_workflow` / `validate_workflow` only
//! validate or mutate a draft and never persist. `dry_run_workflow` delegates
//! to the `SandboxRunner` port; production hosts route it through
//! `Runs.SandboxRun`, where only a ToolInvoke preflight callback crosses the
//! bus and the module produces its own mock echo. It is not tier-gated (F7).
//! [`ResumeFlowRunTool`] is the one place a non-persistence tool can cause a
//! real effect: resuming a parked run lets its already-approved nodes fire,
//! which is why it is `Execute`-gated rather than `None`/`Write`.
//!
//!
//! The Composio-bound reads (`search_tool_catalog`, `get_tool_contract`,
//! `get_tool_output_sample`, `list_connectable_toolkits`,
//! `list_flow_connections`) stay with the host that owns the live catalog; they
//! sit beside these tools in the same belt but are not defined here.
//!
//! Every tool runs over the ports in [`ports`]; none of them names a store, a
//! config type or a harness.

pub mod ports;

mod discovery;
mod draft_edit;
mod draft_revise;
mod draft_validate;
mod dry_run;
mod flow_reads;
mod kind_reads;
mod persistence;
mod propose;
mod run_control;
mod run_flow;

pub use discovery::SuggestWorkflowsTool;
pub use draft_edit::{EditWorkflowTool, edit_op_shape};
pub use draft_revise::ReviseWorkflowTool;
pub use draft_validate::ValidateWorkflowTool;
pub use dry_run::DryRunWorkflowTool;
pub use flow_reads::{
    GetFlowHistoryTool, GetFlowRunTool, GetFlowTool, ListFlowRunsTool, ListFlowsTool,
};
pub use kind_reads::{GetNodeKindContractTool, ListAgentDefinitionsTool, ListNodeKindsTool};
pub use persistence::{CreateWorkflowTool, DuplicateFlowTool, SaveWorkflowTool};
pub use propose::ProposeWorkflowTool;
pub use run_control::{CancelFlowRunTool, ResumeFlowRunTool};
pub use run_flow::RunFlowTool;

/// Return a constant tool schema parsed once from an embedded JSON string.
macro_rules! static_schema {
    ($src:expr) => {{
        static SCHEMA: ::std::sync::LazyLock<::serde_json::Value> =
            ::std::sync::LazyLock::new(|| match ::serde_json::from_str($src) {
                Ok(value) => value,
                Err(error) => panic!(
                    "embedded tool schema at {}:{} is not valid JSON: {error}",
                    file!(),
                    line!()
                ),
            });
        ::std::clone::Clone::clone(&*SCHEMA)
    }};
}

pub(crate) use static_schema;

#[cfg(test)]
pub(crate) mod fakes;
#[cfg(test)]
#[path = "tools_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "dry_run_port_tests.rs"]
mod dry_run_port_tests;
