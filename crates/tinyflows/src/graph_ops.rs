//! Structured, incremental edits to a [`WorkflowGraph`] — the patch-op layer.
//!
//! Authoring a flow by re-emitting the *entire* graph on every change is
//! token-heavy and the #1 source of accidental regressions (a dropped node, a
//! mangled edge). This module lets a caller express a change as a small list of
//! [`GraphOp`]s — add a node, merge-patch one node's config, rewire an edge —
//! applied to a base graph with precise, per-op errors.
//!
//! [`apply_ops`] performs only the **structural mutation**; it deliberately
//! does not run [`crate::validate`]. The intended pipeline is
//! `apply_ops` → `validate_all` → (host gates), so a caller gets a clear
//! "op 3 (add_edge) failed" for a malformed *operation* separately from the
//! structural validation of the resulting graph.
//!
//! Host-agnostic: these are edits to the portable model, with no knowledge of
//! what any `config` field means.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::model::{Edge, Node, NodeId, Position, WorkflowGraph};

/// Serde default for an op's port fields — matches [`Edge`]'s `"main"` default.
fn default_port() -> String {
    "main".to_string()
}

/// One structured edit to a [`WorkflowGraph`].
///
/// Serialized as an internally-tagged object: `{ "op": "add_node", ... }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum GraphOp {
    /// Append a new node. Fails if its `id` is empty or already present.
    AddNode {
        /// The node to add.
        node: Node,
    },
    /// Merge-patch a node's `config` (RFC 7386 JSON Merge Patch): keys in
    /// `config` are recursively merged onto the node's existing config, and a
    /// `null` value deletes that key. Fails if no node has `id`.
    UpdateNodeConfig {
        /// The target node id. Accepts the alias `node_id`.
        #[serde(alias = "node_id")]
        id: NodeId,
        /// The partial config to merge (a `null` leaf deletes the key).
        config: Value,
    },
    /// Replace a node's human-readable `name`. Fails if no node has `id`.
    SetNodeName {
        /// The target node id. Accepts the alias `node_id`.
        #[serde(alias = "node_id")]
        id: NodeId,
        /// The new display name.
        name: String,
    },
    /// Change a node's `id`, rewiring every edge that referenced the old id.
    ///
    /// Note: `=nodes.<id>…` references inside *other* nodes' config expressions
    /// are NOT rewritten (that would require parsing jq) — a caller renaming a
    /// node that others bind to should re-point those bindings itself. Fails if
    /// `new_id` is empty or already in use, or if no node has `id`.
    RenameNode {
        /// The current node id. Accepts the alias `node_id`.
        #[serde(alias = "node_id")]
        id: NodeId,
        /// The new node id. Accepts the alias `new_node_id`.
        #[serde(alias = "new_node_id")]
        new_id: NodeId,
    },
    /// Remove a node and every edge incident on it. Fails if no node has `id`.
    RemoveNode {
        /// The node id to remove. Accepts the alias `node_id`.
        #[serde(alias = "node_id")]
        id: NodeId,
    },
    /// Add a directed edge. Fails if either endpoint node is missing or the
    /// exact edge (same `from`/`to` node and port) already exists.
    AddEdge {
        /// The edge to add.
        edge: Edge,
    },
    /// Remove every edge matching the given `from`/`to` node and port (ports
    /// default to `"main"`). Fails if no edge matches.
    RemoveEdge {
        /// Source node id.
        from_node: NodeId,
        /// Source port (defaults to `"main"`).
        #[serde(default = "default_port")]
        from_port: String,
        /// Target node id.
        to_node: NodeId,
        /// Target port (defaults to `"main"`).
        #[serde(default = "default_port")]
        to_port: String,
    },
    /// Set (or move) a node's canvas position. Fails if no node has `id`.
    SetNodePosition {
        /// The target node id. Accepts the alias `node_id`.
        #[serde(alias = "node_id")]
        id: NodeId,
        /// The new canvas position.
        position: Position,
    },
    /// Replace the workflow's declared inputs wholesale.
    ///
    /// A whole-list replace rather than per-input add/remove/update ops: the
    /// list is short, order is meaningful (it drives the order a host prompts
    /// for values), and a rename is otherwise two ops that must not be
    /// interleaved with anything else. Never fails structurally — the resulting
    /// declarations are checked by [`crate::validate::validate_all`], which
    /// reports duplicate names, unaddressable names, and self-contradictory
    /// defaults.
    SetWorkflowInputs {
        /// The complete new set of declared inputs (empty clears them).
        inputs: Vec<crate::model::WorkflowInput>,
    },
}

impl GraphOp {
    /// The op's stable, machine-readable name (its serde tag), for diagnostics.
    pub fn name(&self) -> &'static str {
        match self {
            Self::AddNode { .. } => "add_node",
            Self::UpdateNodeConfig { .. } => "update_node_config",
            Self::SetNodeName { .. } => "set_node_name",
            Self::RenameNode { .. } => "rename_node",
            Self::RemoveNode { .. } => "remove_node",
            Self::AddEdge { .. } => "add_edge",
            Self::RemoveEdge { .. } => "remove_edge",
            Self::SetNodePosition { .. } => "set_node_position",
            Self::SetWorkflowInputs { .. } => "set_workflow_inputs",
        }
    }
}

/// A 4-tuple identifying an edge, for edge-related errors. Boxed inside
/// [`GraphOpErrorKind`] so those variants stay small.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeRef {
    /// Source node id.
    pub from_node: NodeId,
    /// Source port.
    pub from_port: String,
    /// Target node id.
    pub to_node: NodeId,
    /// Target port.
    pub to_port: String,
}

impl std::fmt::Display for EdgeRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}.{} -> {}.{}",
            self.from_node, self.from_port, self.to_node, self.to_port
        )
    }
}

/// Why a single [`GraphOp`] could not be applied.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GraphOpErrorKind {
    /// A referenced node id does not exist in the graph.
    #[error("no node with id {0}")]
    NodeNotFound(NodeId),
    /// A node id that must be new is already taken.
    #[error("a node with id {0} already exists")]
    NodeIdExists(NodeId),
    /// A node id (new or renamed-to) is empty.
    #[error("node id must not be empty")]
    EmptyNodeId,
    /// An edge endpoint references a node that does not exist.
    #[error("edge references unknown node id {0}")]
    EdgeEndpointMissing(NodeId),
    /// The exact edge already exists.
    #[error("edge {0} already exists")]
    EdgeExists(Box<EdgeRef>),
    /// No edge matched a [`GraphOp::RemoveEdge`].
    #[error("no edge {0} to remove")]
    EdgeNotFound(Box<EdgeRef>),
}

/// A failure to apply an op, carrying which op (0-based index) failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("op {index} ({op}): {kind}")]
pub struct GraphOpError {
    /// 0-based index of the failing op in the input list.
    pub index: usize,
    /// The failing op's name (serde tag).
    pub op: &'static str,
    /// What went wrong.
    pub kind: GraphOpErrorKind,
}

/// Applies `ops` to a clone of `base`, in order, returning the mutated graph.
///
/// Purely structural: on the first op that cannot be applied, returns a
/// [`GraphOpError`] naming the op index and reason, leaving `base` untouched
/// (the working copy is discarded). Run [`crate::validate::validate_all`] on
/// the result to check the resulting graph's structure.
pub fn apply_ops(base: &WorkflowGraph, ops: &[GraphOp]) -> Result<WorkflowGraph, GraphOpError> {
    let mut graph = base.clone();
    for (index, op) in ops.iter().enumerate() {
        apply_one(&mut graph, op).map_err(|kind| GraphOpError {
            index,
            op: op.name(),
            kind,
        })?;
    }
    Ok(graph)
}

fn node_index(graph: &WorkflowGraph, id: &str) -> Option<usize> {
    graph.nodes.iter().position(|n| n.id == id)
}

fn apply_one(graph: &mut WorkflowGraph, op: &GraphOp) -> Result<(), GraphOpErrorKind> {
    match op {
        GraphOp::AddNode { node } => {
            if node.id.is_empty() {
                return Err(GraphOpErrorKind::EmptyNodeId);
            }
            if node_index(graph, &node.id).is_some() {
                return Err(GraphOpErrorKind::NodeIdExists(node.id.clone()));
            }
            graph.nodes.push(node.clone());
        }
        GraphOp::UpdateNodeConfig { id, config } => {
            let idx =
                node_index(graph, id).ok_or_else(|| GraphOpErrorKind::NodeNotFound(id.clone()))?;
            json_merge_patch(&mut graph.nodes[idx].config, config);
        }
        GraphOp::SetNodeName { id, name } => {
            let idx =
                node_index(graph, id).ok_or_else(|| GraphOpErrorKind::NodeNotFound(id.clone()))?;
            graph.nodes[idx].name = name.clone();
        }
        GraphOp::RenameNode { id, new_id } => {
            if new_id.is_empty() {
                return Err(GraphOpErrorKind::EmptyNodeId);
            }
            if node_index(graph, id).is_none() {
                return Err(GraphOpErrorKind::NodeNotFound(id.clone()));
            }
            if new_id != id && node_index(graph, new_id).is_some() {
                return Err(GraphOpErrorKind::NodeIdExists(new_id.clone()));
            }
            for node in &mut graph.nodes {
                if node.id == *id {
                    node.id = new_id.clone();
                }
            }
            for edge in &mut graph.edges {
                if edge.from_node == *id {
                    edge.from_node = new_id.clone();
                }
                if edge.to_node == *id {
                    edge.to_node = new_id.clone();
                }
            }
        }
        GraphOp::RemoveNode { id } => {
            if node_index(graph, id).is_none() {
                return Err(GraphOpErrorKind::NodeNotFound(id.clone()));
            }
            graph.nodes.retain(|n| n.id != *id);
            graph
                .edges
                .retain(|e| e.from_node != *id && e.to_node != *id);
        }
        GraphOp::AddEdge { edge } => {
            if node_index(graph, &edge.from_node).is_none() {
                return Err(GraphOpErrorKind::EdgeEndpointMissing(
                    edge.from_node.clone(),
                ));
            }
            if node_index(graph, &edge.to_node).is_none() {
                return Err(GraphOpErrorKind::EdgeEndpointMissing(edge.to_node.clone()));
            }
            let exists = graph.edges.iter().any(|e| {
                e.from_node == edge.from_node
                    && e.from_port == edge.from_port
                    && e.to_node == edge.to_node
                    && e.to_port == edge.to_port
            });
            if exists {
                return Err(GraphOpErrorKind::EdgeExists(Box::new(EdgeRef {
                    from_node: edge.from_node.clone(),
                    from_port: edge.from_port.clone(),
                    to_node: edge.to_node.clone(),
                    to_port: edge.to_port.clone(),
                })));
            }
            graph.edges.push(edge.clone());
        }
        GraphOp::RemoveEdge {
            from_node,
            from_port,
            to_node,
            to_port,
        } => {
            let before = graph.edges.len();
            graph.edges.retain(|e| {
                !(e.from_node == *from_node
                    && e.from_port == *from_port
                    && e.to_node == *to_node
                    && e.to_port == *to_port)
            });
            if graph.edges.len() == before {
                return Err(GraphOpErrorKind::EdgeNotFound(Box::new(EdgeRef {
                    from_node: from_node.clone(),
                    from_port: from_port.clone(),
                    to_node: to_node.clone(),
                    to_port: to_port.clone(),
                })));
            }
        }
        GraphOp::SetNodePosition { id, position } => {
            let idx =
                node_index(graph, id).ok_or_else(|| GraphOpErrorKind::NodeNotFound(id.clone()))?;
            graph.nodes[idx].position = Some(*position);
        }
        GraphOp::SetWorkflowInputs { inputs } => {
            graph.inputs = inputs.clone();
        }
    }
    Ok(())
}

/// Applies an RFC 7386 JSON Merge Patch of `patch` onto `target` in place.
///
/// Object values are merged recursively; a `null` leaf deletes the
/// corresponding key; any non-object patch replaces the target wholesale.
fn json_merge_patch(target: &mut Value, patch: &Value) {
    match patch {
        Value::Object(patch_map) => {
            if !target.is_object() {
                *target = Value::Object(Map::new());
            }
            let target_map = target.as_object_mut().expect("just ensured object");
            for (key, patch_val) in patch_map {
                if patch_val.is_null() {
                    target_map.remove(key);
                } else {
                    json_merge_patch(
                        target_map.entry(key.clone()).or_insert(Value::Null),
                        patch_val,
                    );
                }
            }
        }
        _ => *target = patch.clone(),
    }
}

#[cfg(test)]
#[path = "graph_ops_tests.rs"]
mod tests;
