//! The tinyflows workflow definition model: a directed graph of typed nodes.
//!
//! A [`WorkflowGraph`] is the serializable source of truth for an automation.
//! Both authoring surfaces — the visual canvas and agent-first chat — produce
//! and edit the *same* `WorkflowGraph`. See `docs/02-workflow-model.md`.

mod node_kind;

pub use node_kind::{NodeKind, TriggerKind};

use serde::{Deserialize, Serialize};

/// Stable identifier for a node within a [`WorkflowGraph`].
pub type NodeId = String;

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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct WorkflowGraph {
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

    /// Returns the ids of every node reachable by following edges from `start`.
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
            name: id.to_string(),
            config: serde_json::Value::Null,
            ports: Vec::new(),
            position: None,
        }
    }

    #[test]
    fn json_round_trips() {
        let graph = WorkflowGraph {
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
