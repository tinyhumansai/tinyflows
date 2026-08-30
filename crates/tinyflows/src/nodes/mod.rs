//! Node execution: the [`NodeExecutor`] trait and per-kind implementations.
//!
//! Each [`crate::model::NodeKind`] maps to a `NodeExecutor`: native control-flow
//! kinds resolve to executors in [`control_flow`], while capability-backed kinds
//! (which reach the outside world via [`crate::caps`]) resolve to executors in
//! [`integration`]. The engine dispatches each node to its executor through
//! `executor_for`.

pub mod control_flow;
pub mod integration;
pub(crate) mod map;
pub(crate) mod release;

use async_trait::async_trait;
use serde_json::Value;

use crate::caps::Capabilities;
use crate::data::Item;
use crate::engine::CancellationToken;
use crate::error::Result;
use crate::model::{Node, NodeKind};

/// The runtime context handed to a node when it executes.
///
/// A node receives its resolved **input items** (the data-flow currency; see
/// [`crate::data`]) plus the run metadata, and returns a [`NodeOutput`].
pub struct NodeContext<'a> {
    /// The node being executed.
    pub node: &'a Node,
    /// The input items delivered to this node, resolved by the engine from the
    /// node's incoming edges. Nodes typically map their logic over these.
    pub input: &'a [Item],
    /// Run metadata and the trigger payload (the `run` slice of the run state).
    pub run: &'a Value,
    /// The `nodes` slice of the run state: every node that has completed so far,
    /// keyed by node id, each slot shaped `{ "items": [<serialized Item>…] }`.
    /// This is what lets an expression address **any** upstream node's output by
    /// id (not just the direct predecessors delivered in `input`). Pass
    /// [`Value::Null`] when no run state exists (e.g. direct executor tests).
    pub nodes: &'a Value,
    /// Host-provided capabilities.
    pub caps: &'a Capabilities,
    /// The graph's own agent registry
    /// ([`WorkflowGraph::agents`](crate::model::WorkflowGraph::agents)).
    ///
    /// An `agent` node resolves its `agent_ref` here first, before falling back
    /// to the harness's registry via
    /// [`AgentRunner::resolve_agent`](crate::caps::AgentRunner::resolve_agent),
    /// so a workflow carrying its own definitions behaves identically on every
    /// host. Empty for a graph that declares none. Every other node kind ignores
    /// it.
    pub agents: &'a [crate::model::AgentDefinition],
    /// Host observer used to report per-item fan-out progress.
    pub observer: &'a dyn crate::observability::RunObserver,
    /// The run's cooperative-cancellation token (see
    /// [`crate::engine::CancellationToken`]). An **owned clone** of the run
    /// token, not a borrow — an executor that spawns nested engine work (today
    /// only [`sub_workflow`](crate::nodes::integration)) must thread a clone
    /// into that child run, so a parent cancel winds the whole subtree down at
    /// the next node boundary instead of orphaning it. Executors that touch the
    /// outside world within a single node need not consult it; the engine
    /// already checks it at the node boundary before this node runs.
    pub token: CancellationToken,
    /// The parallel lane this activation belongs to, when it is one of several
    /// concurrent activations of the *same* node produced by a fan-out.
    ///
    /// `None` for an ordinary activation, which is every activation today. A
    /// lane activation takes its input from the lane envelope rather than from
    /// its predecessors' run-state slots, because every lane sees the same
    /// committed state snapshot and would otherwise all read identical items.
    pub lane: Option<LaneContext>,
    /// The value a checkpointed resume delivered to this node, when this
    /// activation is the re-run of a node that had interrupted.
    ///
    /// `None` on an ordinary activation. A node that pauses the run with
    /// [`NodeControl::Interrupt`] reads this on its re-run to learn what the
    /// host decided — which is the only channel on the checkpointed resume path,
    /// since that path replays from the checkpoint rather than re-executing with
    /// a merged run input.
    pub resume: Option<Value>,
    /// The super-step that is running this activation, counting from 0.
    ///
    /// A monotonic tick that does not depend on the wall clock, so it stays
    /// meaningful across a checkpointed resume. A node that must recognise its
    /// own earlier activations (a poll budget, a fold that must not re-apply)
    /// compares against a step it recorded in its slot.
    pub step: usize,
}

/// Which lane of a fan-out an activation belongs to.
///
/// Travels with the activation itself rather than through the run state, so N
/// concurrent activations of one node id are told apart without any of them
/// having to write a shared slot.
#[derive(Debug, Clone, PartialEq)]
pub struct LaneContext {
    /// Unique across the run; conventionally `"<fan-out node id>#<index>"`.
    pub id: String,
    /// The node whose fan-out created this lane.
    pub origin: String,
    /// This lane's position in the fan-out, from 0. Output is ordered by this
    /// rather than by completion, so a run is deterministic under any timing.
    pub index: usize,
    /// How many lanes the fan-out created, which is what a collector counts
    /// arrivals against.
    pub count: usize,
}

/// Builds the expression scope for a node from its runtime [`NodeContext`].
///
/// The returned object is the `.` input for `=`-expressions evaluated over a
/// node's config (see [`crate::expr::resolve`]). It exposes exactly what
/// `NodeContext` makes available:
///
/// - `item` — the first input item's `json`, or [`Value::Null`] when there is
///   no input;
/// - `items` — the `json` of every input item, in order;
/// - `run` — the run metadata / trigger payload (`ctx.run`);
/// - `nodes` — every **completed** node's output, keyed by node id, each entry
///   shaped `{ "item": <first json>, "items": [<json>…] }`. This lets an
///   expression reference any upstream node by id — e.g.
///   `=nodes.fetch_recipient.item.email` or jq
///   `=.nodes["fetch_recipient"].items[0].email` — including non-adjacent
///   (grandparent) nodes and specific predecessors of a fan-in node. Node
///   **id** is the addressing key (stable across renames); names are not
///   indexed. A node that recorded slot state about itself also exposes it
///   here — currently only a `loop` node's `iteration`, so
///   `=nodes.my_loop.iteration` is the current pass number;
/// - `inputs` — the workflow's resolved declared inputs, keyed by name (see
///   [`crate::model::WorkflowInput`]), so a config field reads
///   `=inputs.repo`. One entry per declaration with defaults already applied,
///   so a binding to a declared name is never *absent* — at worst it is the
///   explicit `null` of an optional input nobody supplied.
///
///   **Write `.inputs.<name>` inside a real jq program.** `=inputs.repo` works
///   because a simple dotted path is walked directly, never compiled. Anything
///   jq actually compiles — a concatenation, a conditional, a pipe — resolves
///   bare `inputs` as jq's own `inputs` *builtin* (which reads further program
///   inputs) rather than this scope key, and the expression quietly yields
///   nothing instead of erroring. The leading dot forces the object lookup:
///   `="Review " + .inputs.repo` is right, `="Review " + inputs.repo` is not.
///   No other scope key has this problem; `inputs` is the one name jq already
///   uses.
#[must_use]
pub(crate) fn expr_scope(ctx: &NodeContext) -> Value {
    let item = ctx
        .input
        .first()
        .map(|i| i.json.clone())
        .unwrap_or(Value::Null);
    expr_scope_for(ctx, item)
}

/// Like [`expr_scope`], but binds `item` to an explicit value rather than the
/// first input item. Used by per-item execution: each iteration resolves the
/// node config against *its own* item, so `=item.<field>` means the current
/// item, while `items`/`run`/`nodes` stay identical across the batch.
#[must_use]
pub(crate) fn expr_scope_for(ctx: &NodeContext, item: Value) -> Value {
    let items: Vec<Value> = ctx.input.iter().map(|i| i.json.clone()).collect();
    build_expr_scope(item, items, ctx.run, nodes_scope(ctx.nodes))
}

/// THE single constructor for an expression scope — every `=`-expression in the
/// crate is evaluated against an object built here.
///
/// Takes an already-projected `nodes` scope so a per-item loop can project it
/// once and reuse it across the batch (see the `transform` node), while callers
/// with a [`NodeContext`] go through [`expr_scope`] / [`expr_scope_for`].
///
/// Keeping this in one place is load-bearing, not tidiness: a node that
/// hand-rolls the object silently loses whatever key it was written before, and
/// the binding fails as a quiet `null` rather than an error. Add a key here and
/// every node sees it.
#[must_use]
pub(crate) fn build_expr_scope(item: Value, items: Vec<Value>, run: &Value, nodes: Value) -> Value {
    serde_json::json!({
        "item": item,
        "items": items,
        "run": run,
        "nodes": nodes,
        // Lifted out of `run` rather than carried separately on `NodeContext`:
        // the engine seeds the resolved declared inputs at `run.inputs`, and
        // this promotes them to a top-level key so authors write the short
        // `=inputs.<name>` while jq programs walking `run` still find them.
        // `Null` when the run predates inputs or the graph declares none.
        "inputs": run.get("inputs").cloned().unwrap_or(Value::Null),
    })
}

mod execution;
pub(crate) use execution::{
    ExecutionMode, execution_mode, executor_for, nodes_scope, resolve_config_traced,
    resolve_config_traced_for_item,
};
pub use execution::{NodeControl, NodeExecutor, NodeOutput};

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
