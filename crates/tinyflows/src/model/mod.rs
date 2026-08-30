//! The tinyflows workflow definition model: a directed graph of typed nodes.
//!
//! A [`WorkflowGraph`] is the serializable source of truth for an automation.
//! Both authoring surfaces — the visual canvas and agent-first chat — produce
//! and edit the *same* `WorkflowGraph`.
//!
//! ## Versioning
//!
//! The JSON wire format is a stable contract. Two version axes make it durable
//! as the model evolves:
//!
//! - [`WorkflowGraph::schema_version`] — the overall model shape. The current
//!   value is [`CURRENT_SCHEMA_VERSION`].
//! - [`Node::type_version`] — the per-kind `config` shape for a node.
//!
//! Both fields are `#[serde(default)]`, so definitions persisted before they
//! existed still load. Load-time upgrades are performed by [`crate::migrate`].
//!
//! ## Inputs
//!
//! A graph also declares its parameters — see [`WorkflowInput`] and
//! [`resolve_inputs`]. They are the workflow's public signature, validated
//! before a run starts and addressed from node config as `=inputs.<name>`.
//!
//! ## Agents
//!
//! A graph may also declare reusable **agent types** — see [`AgentDefinition`]
//! and [`WorkflowGraph::agents`]. An `agent` node selects one by `agent_ref` and
//! may narrow it (tighter limits, fewer tools, extra instructions), so one
//! definition serves many nodes and travels with the workflow between hosts.

mod agent;
mod inputs;
mod node_kind;

pub use agent::{AgentDefinition, AgentLimits, ContextSource, ContextSourceKind, ToolGrant};
pub use inputs::{InputError, InputType, WorkflowInput, is_valid_input_name, resolve_inputs};
pub use node_kind::{NodeKind, TriggerKind};

use serde::{Deserialize, Serialize};

/// The current [`WorkflowGraph`] schema version understood by this crate.
///
/// Graphs persisted with a lower `schema_version` are upgraded on load by
/// [`crate::migrate`]. Bumping this constant is a breaking JSON-format change
/// and must ship with a migration.
///
/// ```
/// assert_eq!(tinyflows::model::CURRENT_SCHEMA_VERSION, 1);
/// ```
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

/// Stable identifier for a node within a [`WorkflowGraph`].
pub type NodeId = String;

/// Serde default for [`WorkflowGraph::schema_version`]: the current schema
/// version, so JSON authored before the field existed loads as up to date.
fn default_schema_version() -> u32 {
    CURRENT_SCHEMA_VERSION
}

/// Serde default for [`Node::type_version`]: the initial version (`1`) for
/// every node kind, so JSON authored before the field existed loads correctly.
fn default_type_version() -> u32 {
    1
}

/// A named input or output connection point on a [`Node`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Port {
    /// The port's stable name (e.g. `"main"`, `"true"`, `"false"`, `"tool"`).
    pub name: String,
    /// Optional human-readable label for the editor.
    #[serde(default)]
    pub label: Option<String>,
}

/// Optional canvas coordinates for a node (ignored by the engine).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct Position {
    /// Horizontal position on the canvas.
    pub x: f64,
    /// Vertical position on the canvas.
    pub y: f64,
}

/// A single unit of work in a workflow.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Node {
    /// Unique id within the graph.
    pub id: NodeId,
    /// The kind of work this node performs.
    pub kind: NodeKind,
    /// Version of this node kind's `config` shape. Defaults to `1`; bumped by a
    /// kind when its configuration evolves, with a per-kind load-time migration.
    #[serde(default = "default_type_version")]
    pub type_version: u32,
    /// Human-readable name shown in the editor.
    pub name: String,
    /// Kind-specific configuration as free-form JSON.
    #[serde(default)]
    pub config: serde_json::Value,
    /// Declared output ports (for branching / multi-output nodes).
    #[serde(default)]
    pub ports: Vec<Port>,
    /// Optional canvas position.
    #[serde(default)]
    pub position: Option<Position>,
}

/// A directed connection from one node's output port to another's input port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edge {
    /// Source node id.
    pub from_node: NodeId,
    /// Source port name (defaults to `"main"`).
    #[serde(default = "default_port")]
    pub from_port: String,
    /// Target node id.
    pub to_node: NodeId,
    /// Target port name (defaults to `"main"`).
    #[serde(default = "default_port")]
    pub to_port: String,
}

fn default_port() -> String {
    "main".to_string()
}

/// A complete, serializable workflow definition.
///
/// A freshly [`Default`](WorkflowGraph::default)-constructed graph is stamped
/// with the [`CURRENT_SCHEMA_VERSION`], and JSON that omits the version fields
/// deserializes with the same defaults, so persisted and in-memory graphs agree:
///
/// ```
/// use tinyflows::model::{WorkflowGraph, CURRENT_SCHEMA_VERSION};
///
/// let fresh = WorkflowGraph::default();
/// assert_eq!(fresh.schema_version, CURRENT_SCHEMA_VERSION);
///
/// // JSON that predates the `schema_version` field still loads as current.
/// let loaded: WorkflowGraph =
///     serde_json::from_str(r#"{"name":"demo","nodes":[],"edges":[]}"#).unwrap();
/// assert_eq!(loaded.schema_version, CURRENT_SCHEMA_VERSION);
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowGraph {
    /// Overall model-shape version. Defaults to [`CURRENT_SCHEMA_VERSION`] so
    /// JSON authored before the field existed loads as the current shape;
    /// older persisted values are upgraded by [`crate::migrate`].
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// Optional stable id of the workflow.
    #[serde(default)]
    pub id: Option<String>,
    /// Human-readable workflow name.
    #[serde(default)]
    pub name: String,
    /// The workflow's declared parameters — its public signature. Empty for
    /// graphs authored before inputs existed, and for graphs that take none.
    ///
    /// Values supplied by a caller are validated against these declarations
    /// before the run starts (see [`resolve_inputs`]) and exposed to node
    /// configuration as `=inputs.<name>`.
    #[serde(default)]
    pub inputs: Vec<WorkflowInput>,
    /// Reusable **agent types** this workflow declares — its own agent
    /// registry, mirroring [`inputs`](Self::inputs).
    ///
    /// An `agent` node's `config.agent_ref` resolves here first, so a workflow
    /// that carries its own definitions behaves identically on every host. A ref
    /// this registry does not declare falls back to the harness's own registry
    /// (see [`AgentRunner::resolve_agent`](crate::caps::AgentRunner::resolve_agent)),
    /// and a ref neither resolves is passed through to the harness as an id.
    ///
    /// Empty for graphs authored before the registry existed, and for graphs
    /// whose agents are entirely host-defined.
    #[serde(default)]
    pub agents: Vec<AgentDefinition>,
    /// The nodes in the graph.
    #[serde(default)]
    pub nodes: Vec<Node>,
    /// The directed edges connecting node ports.
    #[serde(default)]
    pub edges: Vec<Edge>,
}

impl Default for WorkflowGraph {
    /// A new, empty graph stamped with the [`CURRENT_SCHEMA_VERSION`] (rather
    /// than `0`), so freshly constructed graphs match freshly deserialized ones.
    fn default() -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            id: None,
            name: String::new(),
            inputs: Vec::new(),
            agents: Vec::new(),
            nodes: Vec::new(),
            edges: Vec::new(),
        }
    }
}

impl WorkflowGraph {
    /// Returns the graph's trigger node, if it has exactly one.
    ///
    /// Returns `None` for a graph with zero triggers *or* more than one, so
    /// callers can treat "not exactly one trigger" uniformly.
    ///
    /// ```
    /// use tinyflows::model::WorkflowGraph;
    ///
    /// // An empty graph has no trigger.
    /// assert!(WorkflowGraph::default().trigger().is_none());
    ///
    /// // A graph deserialized with a single trigger returns it.
    /// let graph: WorkflowGraph = serde_json::from_str(
    ///     r#"{"nodes":[{"id":"t","kind":"trigger","name":"start"}],"edges":[]}"#,
    /// )
    /// .unwrap();
    /// assert_eq!(graph.trigger().map(|n| n.id.as_str()), Some("t"));
    /// ```
    #[must_use]
    pub fn trigger(&self) -> Option<&Node> {
        let mut triggers = self.nodes.iter().filter(|n| n.kind == NodeKind::Trigger);
        let first = triggers.next()?;
        match triggers.next() {
            Some(_) => None,
            None => Some(first),
        }
    }

    /// Looks up a node by id.
    #[must_use]
    pub fn node(&self, id: &str) -> Option<&Node> {
        self.nodes.iter().find(|n| n.id == id)
    }

    /// Looks up an in-graph [`AgentDefinition`] by id.
    ///
    /// Returns `None` when the graph declares no such agent — a normal outcome,
    /// not a failure: the `agent` node then falls back to the harness's registry
    /// and finally to passing the ref through as an id.
    ///
    /// ```
    /// use tinyflows::model::WorkflowGraph;
    ///
    /// let graph: WorkflowGraph = serde_json::from_str(
    ///     r#"{"agents":[{"id":"triager","model":"claude-opus-5"}],"nodes":[],"edges":[]}"#,
    /// )
    /// .unwrap();
    /// assert_eq!(graph.agent("triager").unwrap().model.as_deref(), Some("claude-opus-5"));
    /// assert!(graph.agent("nobody").is_none());
    /// ```
    #[must_use]
    pub fn agent(&self, id: &str) -> Option<&AgentDefinition> {
        self.agents.iter().find(|a| a.id == id)
    }

    /// Returns the ids of the **direct** successors of `start` — the target node
    /// of each edge leaving it (immediate neighbors only, not the transitive
    /// closure; ids may repeat if multiple edges connect the same pair).
    ///
    /// ```
    /// use tinyflows::model::WorkflowGraph;
    ///
    /// let graph: WorkflowGraph = serde_json::from_str(
    ///     r#"{
    ///       "nodes":[
    ///         {"id":"t","kind":"trigger","name":"start"},
    ///         {"id":"a","kind":"agent","name":"a"}
    ///       ],
    ///       "edges":[{"from_node":"t","to_node":"a"}]
    ///     }"#,
    /// )
    /// .unwrap();
    /// assert_eq!(graph.successors("t"), vec!["a"]);
    /// assert!(graph.successors("a").is_empty());
    /// ```
    #[must_use]
    pub fn successors(&self, start: &str) -> Vec<&str> {
        self.edges
            .iter()
            .filter(|e| e.from_node == start)
            .map(|e| e.to_node.as_str())
            .collect()
    }
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
