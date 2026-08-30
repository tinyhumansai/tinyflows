//! The `agent` node's host capability: running a host-owned agent.
//!
//! tinyflows implements **no agentic loop**. The model↔tool loop, the
//! transcript, the budget, and the prompt assembly all belong to the embedding
//! harness. This crate's contribution is the *assembly*: turning an author's
//! declarative [`AgentDefinition`], context sources, and tool grants into one
//! inert, fully-resolved [`AgentRunRequest`], and turning the harness's
//! [`AgentRunOutcome`] back into the stable `{ json, text, raw, meta }` item
//! envelope.
//!
//! ```text
//!   graph `agents` registry ─┐
//!   node config             ─┼─► merge ─► resolve context ─► resolve tools
//!   AgentRunner::resolve_*  ─┘                                     │
//!                                                                  ▼
//!                        AgentRunOutcome ◄── AgentRunner::run(AgentRunRequest)
//! ```
//!
//! ## Everything new here is additive
//!
//! [`AgentRunner::run_agent`] is unchanged and remains the trait's only required
//! method, so a host written against the previous release compiles and behaves
//! identically: the default [`run`](AgentRunner::run) replays exactly the call
//! it made before. A harness opts into richer agents by overriding the defaulted
//! methods, one at a time.
//!
//! ## Absence is not failure
//!
//! [`resolve_agent`](AgentRunner::resolve_agent) and
//! [`resolve_context`](AgentRunner::resolve_context) return `Ok(None)` for an
//! id they do not recognize. That is a **normal outcome**, not a soft error: a
//! host's catalogue is data and legitimately names things absent from a given
//! build, and the caller has a fallback. `Err` is reserved for a genuine failure
//! to *answer* — a broken store, a lost connection.
//!
//! Every value type in this module is `serde` + `std` only, so a host can write
//! an adapter against it without linking the engine.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::error::Result;
use crate::model::{AgentDefinition, ToolGrant};

/// Which model, on which provider, an agent run should use.
///
/// Both fields are opaque to the engine and both may be `None`, meaning "no
/// author preference — the harness's default applies". They are carried
/// separately rather than as one composite string so a harness routing the same
/// model through different providers need not parse them apart.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AgentModelSelection {
    /// Model id, e.g. `"claude-opus-5"` or a host alias.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Provider id, e.g. `"anthropic"`, a gateway name, or a host routing key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

impl AgentModelSelection {
    /// Whether the author expressed no preference at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.model.is_none() && self.provider.is_none()
    }
}

/// What the agent is being asked to do.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AgentInput {
    /// The task text (the node's `prompt`), with `=`-expressions resolved.
    ///
    /// An empty string is legal: a run driven entirely by `data` and context is
    /// a real case.
    #[serde(default)]
    pub text: String,
    /// The `json` payload of each of the node's input items, in order.
    ///
    /// Carried alongside `text` rather than stringified into it, so a harness
    /// with structured input support need not parse prose back apart, and one
    /// without it can serialize this however it likes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub data: Vec<Value>,
}

/// One resolved block of agent context, ready to hand to the harness.
///
/// The engine resolves each declared
/// [`ContextSource`](crate::model::ContextSource) in declaration order and
/// passes the list through. It never reorders, merges, de-duplicates, ranks, or
/// truncates blocks — composing them into a prompt is the harness's job,
/// because the harness owns the loop.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ContextBlock {
    /// The authored label, or the generated `"context_<n>"`.
    pub label: String,
    /// The wire name of the source kind this came from (`"text"`, `"items"`,
    /// `"memory"`, `"flavour"`, `"host"`), so a harness can format a memory
    /// recall differently from literal text without re-deriving where it came
    /// from.
    pub source_kind: String,
    /// Human-readable body, when the source produced prose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Structured body, when the source produced data — a memory `results`
    /// array, a host record. [`Value::Null`] when there is none.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub data: Value,
}

impl ContextBlock {
    /// A prose block.
    #[must_use]
    pub fn text(label: impl Into<String>, source_kind: &str, text: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            source_kind: source_kind.to_string(),
            text: Some(text.into()),
            data: Value::Null,
        }
    }

    /// A structured block.
    #[must_use]
    pub fn data(label: impl Into<String>, source_kind: &str, data: Value) -> Self {
        Self {
            label: label.into(),
            source_kind: source_kind.to_string(),
            text: None,
            data,
        }
    }
}

/// A concrete tool the harness may offer the model this run.
///
/// The discovery half [`ToolInvoker`](crate::caps::ToolInvoker) never had: a
/// slug plus enough metadata for a model to choose it. Produced from an
/// author's [`ToolGrant`]s by
/// [`AgentRunner::resolve_tools`](AgentRunner::resolve_tools).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ToolDescriptor {
    /// The slug the harness would pass to
    /// [`ToolInvoker::invoke`](crate::caps::ToolInvoker::invoke).
    ///
    /// Concrete when a harness expanded a grant; a harness that does not expand
    /// receives the author's grant verbatim, patterns included.
    pub slug: String,
    /// Display name, when the harness knows one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// What the tool does, for the model's benefit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema for the tool's arguments. `None` means "undescribed", not
    /// "takes no arguments".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<Value>,
    /// The trusted connection reference this tool must be called with, carried
    /// down from the grant or the node. Never model-supplied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_ref: Option<String>,
}

impl ToolDescriptor {
    /// The undescribed descriptor a [`ToolGrant`] becomes when no harness
    /// expands it: the slug and its trusted connection reference, nothing more.
    #[must_use]
    pub fn from_grant(grant: &ToolGrant, fallback_conn: Option<&str>) -> Self {
        Self {
            slug: grant.slug.clone(),
            name: None,
            description: None,
            input_schema: None,
            connection_ref: grant
                .connection_ref
                .clone()
                .or_else(|| fallback_conn.map(str::to_string)),
        }
    }
}

/// Who is running, for host-side tracing, budgeting, and rate limiting.
///
/// Informational to the engine — nothing here is enforced, and a harness may
/// ignore all of it. It exists because a harness that cannot attribute an agent
/// run to a workflow run cannot bill it, trace it, or cap it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AgentRunIdentity {
    /// The enclosing workflow run's id, when the run has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// The workflow's id, when the graph declares one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_id: Option<String>,
    /// The graph id of the `agent` node issuing this request. Stable across
    /// renames, so it is the right key for a per-node budget.
    pub node_id: String,
    /// `sub_workflow` nesting depth, `0` at the top level. A harness budgeting
    /// recursive workflows needs this to tell a runaway chain from a wide one.
    #[serde(default)]
    pub depth: u32,
    /// Index of the input item this run is for under `per_item` execution;
    /// `None` for a single `once` turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_index: Option<usize>,
}

/// One `agent` node's run request: everything the harness needs, fully
/// resolved.
///
/// By the time a harness sees one, every `=`-expression is evaluated, every
/// context block is materialized, and every tool grant has been offered for
/// expansion. The harness resolves nothing further except its own credentials.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentRunRequest {
    /// The merged, fully-resolved agent type: id, instructions, limits, and the
    /// grants and context sources it was defined with.
    pub agent: AgentDefinition,
    /// Which model, on which provider.
    #[serde(default)]
    pub model: AgentModelSelection,
    /// What this run is being asked to do.
    #[serde(default)]
    pub input: AgentInput,
    /// Resolved context, in declaration order — the definition's blocks first,
    /// then the node's. Empty is normal.
    #[serde(default)]
    pub context: Vec<ContextBlock>,
    /// The tools this run may use, already narrowed by any node-level
    /// allow-list.
    ///
    /// An empty vec means **no tools**, not "harness default"; a harness must
    /// not widen it.
    #[serde(default)]
    pub tools: Vec<ToolDescriptor>,
    /// The opaque, host-managed connection reference naming the account this
    /// run acts as.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_ref: Option<String>,
    /// Working directory the agent should run in, when it runs somewhere with a
    /// filesystem. `None` means the harness's default applies.
    ///
    /// Surfaced here as well as on
    /// [`agent.working_dir`](crate::model::AgentDefinition::working_dir)
    /// because a harness reaching for it should not have to know whether the
    /// definition or the node supplied it — this is the merged, effective value.
    ///
    /// **Untrusted authoring input**: the engine never touches the filesystem
    /// and never checks that this exists or is permitted. Validate it against
    /// your own allowed roots before handing it to a process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    /// Run identity, for host-side tracing and budgeting.
    #[serde(default)]
    pub identity: AgentRunIdentity,
    /// Free-form passthrough: the agent definition's `metadata` with the node's
    /// merged over it. The engine never interprets it.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub metadata: Map<String, Value>,
    /// The schema the node's `output_parser` sub-port will validate against.
    ///
    /// **Advisory.** The engine still validates and repairs the outcome itself,
    /// so a harness may ignore this. It is forwarded because a harness with
    /// native structured output gets the right shape on the first try instead of
    /// paying for a repair round-trip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
    /// The node's resolved config, verbatim.
    ///
    /// This is what the default [`AgentRunner::run`] forwards to
    /// [`run_agent`](AgentRunner::run_agent), which is how a host written
    /// against the previous release keeps receiving byte-identical input. It is
    /// also the escape hatch for any config key this struct does not model.
    #[serde(default)]
    pub config: Value,
}

mod outcome;
pub use outcome::{AgentRunOutcome, AgentUsage, StopReason};

mod runner;
pub use runner::{AgentRunner, WorkdirCheck};

#[cfg(test)]
#[path = "agent_tests.rs"]
mod tests;
