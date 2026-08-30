//! Node-kind discriminators for the workflow model.

use serde::{Deserialize, Serialize};

/// The category of work a [`crate::model::Node`] performs.
///
/// Kind-specific configuration lives in [`crate::model::Node::config`] as
/// free-form JSON, validated per kind during compilation. Variants serialize as
/// `snake_case` strings on the JSON wire:
///
/// ```
/// use tinyflows::model::NodeKind;
///
/// assert_eq!(serde_json::to_string(&NodeKind::HttpRequest).unwrap(), "\"http_request\"");
/// let kind: NodeKind = serde_json::from_str("\"split_out\"").unwrap();
/// assert_eq!(kind, NodeKind::SplitOut);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    /// Entry node that starts the workflow. The trigger's firing mode is carried
    /// in config as a [`TriggerKind`].
    Trigger,
    /// Runs an LLM agent turn, with optional chat-model / memory / tool /
    /// output-parser sub-ports.
    Agent,
    /// Invokes one specific integration action (a curated Composio tool).
    ToolCall,
    /// Performs an outbound HTTP request.
    HttpRequest,
    /// Executes sandboxed user code (JavaScript or Python).
    Code,
    /// Executes an inline POSIX-compatible shell script through the host's
    /// explicitly configured code capability.
    Shell,
    /// Two-way conditional branch, emitting on the `true` or `false` port.
    Condition,
    /// Multi-way branch keyed by an expression result.
    Switch,
    /// Fan-in barrier that combines multiple named inputs.
    Merge,
    /// Fan-out that emits one item per element of a list.
    SplitOut,
    /// Bounded loop head: emits its input on the `body` port until its
    /// `max_iterations` cap (or its optional `condition`) says otherwise, then
    /// emits on `done`. The loop is closed by wiring the last node of the body
    /// back to this node; that closing edge is a back-edge and the engine
    /// lowers it accordingly (see [`crate::engine`]), so the head is re-entered
    /// rather than barriered. The iteration count lives in the node's own run
    /// state slot, which makes it survive checkpoint/resume and addressable
    /// from expressions as `=nodes.<id>.iteration`.
    Loop,
    /// Pure, expression-based data transform over the run state.
    Transform,
    /// Parses and validates an upstream agent's output into a structured shape.
    OutputParser,
    /// Runs another workflow as a nested sub-graph.
    SubWorkflow,
    /// Reads or writes host-managed memory via the injected
    /// [`MemoryProvider`](crate::caps::MemoryProvider) capability: `recall` /
    /// `search` / `flavour` / `people` for reads, `remember` / `forget` for
    /// writes. Additive kind — see [`crate::validate`] for the hard invariant
    /// that a `remember`/`forget` operation may never target `scope: "user"`.
    Memory,
    /// Commit-on-success exactly-once **filter** half: drops an item whose
    /// resolved `config.key` was already committed by a prior successful run,
    /// and otherwise passes it through while recording the key as *tentative*
    /// (pending the host's commit on run success). See
    /// [`crate::nodes::control_flow::dedup`] for the full `StateStore` key
    /// contract this kind depends on.
    Dedup,
    /// Starts work **without waiting for it** and emits a ticket per started
    /// task, so the branch carries on while the work runs. A downstream
    /// [`NodeKind::Gate`] collects the results.
    ///
    /// Needs the [`TaskRunner`](crate::caps::TaskRunner) capability to actually
    /// overlap; with none injected the work runs inline and the ticket comes
    /// back already settled, so the graph still computes the right answer
    /// without the concurrency.
    Spawn,
    /// Fans the **downstream path** out into parallel lanes: each lane runs its
    /// own copy of every node between here and the matching [`NodeKind::Gather`].
    ///
    /// Different from an ordinary fan-out, which runs each *successor* once. A
    /// scatter over 8 items turns a five-node pipeline into 8 concurrent
    /// five-node pipelines.
    Scatter,
    /// Collects the lanes a [`NodeKind::Scatter`] opened, on a release policy.
    ///
    /// Where lanes end: routing to a gather is a plain activation, so N lanes
    /// converge on one gather rather than each running their own.
    Gather,
    /// Presents a subject — a URL, a block of text, a generated payload — to a
    /// **human** for an approve/reject, and routes on their answer.
    ///
    /// Distinct from the `requires_approval` flag, which gates a node that
    /// would otherwise run: this *is* the review step, it carries what is being
    /// reviewed, and its verdict is data (who decided, their comment, any edit
    /// they made) that the graph can branch on via its `approved` / `rejected`
    /// ports.
    ///
    /// Reaches the human through the optional
    /// [`ApprovalProvider`](crate::caps::ApprovalProvider) capability. With none
    /// injected it falls back to pausing the run, so the host settles it with
    /// [`engine::resume`](crate::engine::resume) the way it already settles an
    /// approval gate.
    Approval,
    /// Waits for tickets — from [`NodeKind::Spawn`], or named by expression —
    /// and emits their results once its release policy is satisfied.
    ///
    /// The policy (`all` / `any` / `first_n` / `quorum` / `timeout_partial`)
    /// is what makes this more than a barrier: a gate can proceed on a quorum
    /// and leave the stragglers, or settle for whatever arrived before its
    /// deadline. See [`crate::nodes::release`].
    Gate,
    /// Terminal sink: accepts items on `main`, discards them, and activates no
    /// successors. The branch ends here, on purpose.
    ///
    /// Purely declarative. A branch could always dead-end — a node with no
    /// outgoing edges is lowered straight to the engine's `END` sentinel — but
    /// an unwired port reads exactly like a forgotten one, so there was no way
    /// to *say* "this is a side effect and nothing waits on it". This kind is
    /// that sentence. It adds no concurrency: the work upstream of it still
    /// runs inline in its own super-step, and only the result is dropped.
    ///
    /// Abandon semantics, identical to an ungathered [`NodeKind::Spawn`]
    /// ticket — nothing is drained or cancelled at run end. `spawn` → `void` is
    /// the explicit spelling of a ticket no [`NodeKind::Gate`] will collect.
    ///
    /// [`crate::validate`] refuses a `void` that has any outgoing edge, or that
    /// has no incoming edge at all.
    Void,
}

/// How a [`NodeKind::Trigger`] node is fired.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerKind {
    /// Fired manually by the user (e.g. a "Run" button).
    Manual,
    /// Fired on a cron / interval schedule.
    Schedule,
    /// Fired by an inbound HTTP webhook.
    Webhook,
    /// Fired by a connected-app event (e.g. a Composio trigger).
    AppEvent,
    /// Fired on form submission.
    Form,
    /// Fired when invoked by another workflow.
    ExecuteByWorkflow,
    /// Fired by an inbound chat message.
    ChatMessage,
    /// Fired by an evaluation run.
    Evaluation,
    /// Fired by a system event (workflow error, file change, and similar).
    System,
}

#[cfg(test)]
#[path = "node_kind_tests.rs"]
mod tests;
