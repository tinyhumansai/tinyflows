//! The tinyflows workflow definition model: a directed graph of typed nodes.
//!
//! A [`WorkflowGraph`] is the serializable source of truth for an automation.
//! Both authoring surfaces — the visual canvas and agent-first chat — produce
//! and edit the *same* `WorkflowGraph`. See `docs/02-workflow-model.md`.
//!
//! ## Versioning
//!
//! The JSON wire format is a stable contract. Two version axes make it durable
//! as the model evolves (see `docs/18-versioning-and-migration.md`):
//!
//! - [`WorkflowGraph::schema_version`] — the overall model shape. The current
//!   value is [`CURRENT_SCHEMA_VERSION`].
//! - [`Node::type_version`] — the per-kind `config` shape for a node.
//!
//! Both fields are `#[serde(default)]`, so definitions persisted before they
//! existed still load. Load-time upgrades are performed by [`crate::migrate`].

mod node_kind;

pub use node_kind::{NodeKind, TriggerKind};

use serde::{Deserialize, Serialize};

/// The current [`WorkflowGraph`] schema version understood by this crate.
///
/// Graphs persisted with a lower `schema_version` are upgraded on load by
/// [`crate::migrate`]. Bumping this constant is a breaking JSON-format change
/// and must ship with a migration (see `docs/18-versioning-and-migration.md`).
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
            nodes: Vec::new(),
            edges: Vec::new(),
        }
    }
}

impl WorkflowGraph {
    /// Returns the graph's trigger node, if it has exactly one.
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

    /// Returns the ids of the **direct** successors of `start` — the target node
    /// of each edge leaving it (immediate neighbors only, not the transitive
    /// closure; ids may repeat if multiple edges connect the same pair).
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
mod tests {
    use super::*;

    fn node(id: &str, kind: NodeKind) -> Node {
        Node {
            id: id.to_string(),
            kind,
            type_version: 1,
            name: id.to_string(),
            config: serde_json::Value::Null,
            ports: Vec::new(),
            position: None,
        }
    }

    #[test]
    fn json_round_trips() {
        let graph = WorkflowGraph {
            schema_version: CURRENT_SCHEMA_VERSION,
            id: Some("wf_1".to_string()),
            name: "demo".to_string(),
            nodes: vec![node("t", NodeKind::Trigger), node("a", NodeKind::Agent)],
            edges: vec![Edge {
                from_node: "t".to_string(),
                from_port: "main".to_string(),
                to_node: "a".to_string(),
                to_port: "main".to_string(),
            }],
        };
        let json = serde_json::to_string(&graph).expect("serialize");
        let back: WorkflowGraph = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(graph, back);
    }

    #[test]
    fn edge_ports_default_to_main() {
        let json = r#"{"from_node":"t","to_node":"a"}"#;
        let edge: Edge = serde_json::from_str(json).expect("deserialize");
        assert_eq!(edge.from_port, "main");
        assert_eq!(edge.to_port, "main");
    }

    #[test]
    fn trigger_and_lookup() {
        let graph = WorkflowGraph {
            nodes: vec![node("t", NodeKind::Trigger), node("a", NodeKind::Agent)],
            ..Default::default()
        };
        assert_eq!(graph.trigger().map(|n| n.id.as_str()), Some("t"));
        assert_eq!(graph.node("a").map(|n| n.id.as_str()), Some("a"));
        assert!(graph.node("missing").is_none());
    }
}
