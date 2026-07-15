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
        /// The target node id.
        id: NodeId,
        /// The partial config to merge (a `null` leaf deletes the key).
        config: Value,
    },
    /// Replace a node's human-readable `name`. Fails if no node has `id`.
    SetNodeName {
        /// The target node id.
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
        /// The current node id.
        id: NodeId,
        /// The new node id.
        new_id: NodeId,
    },
    /// Remove a node and every edge incident on it. Fails if no node has `id`.
    RemoveNode {
        /// The node id to remove.
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
        /// The target node id.
        id: NodeId,
        /// The new canvas position.
        position: Position,
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
mod tests {
    use super::*;
    use crate::model::NodeKind;
    use serde_json::json;

    fn node(id: &str, kind: NodeKind) -> Node {
        Node {
            id: id.to_string(),
            kind,
            type_version: 1,
            name: id.to_string(),
            config: Value::Null,
            ports: Vec::new(),
            position: None,
        }
    }

    fn base() -> WorkflowGraph {
        WorkflowGraph {
            nodes: vec![node("t", NodeKind::Trigger), node("a", NodeKind::Agent)],
            edges: vec![Edge {
                from_node: "t".to_string(),
                from_port: "main".to_string(),
                to_node: "a".to_string(),
                to_port: "main".to_string(),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn add_node_appends_and_rejects_duplicates() {
        let g = apply_ops(
            &base(),
            &[GraphOp::AddNode {
                node: node("b", NodeKind::Merge),
            }],
        )
        .unwrap();
        assert_eq!(g.nodes.len(), 3);

        let err = apply_ops(
            &base(),
            &[GraphOp::AddNode {
                node: node("a", NodeKind::Agent),
            }],
        )
        .unwrap_err();
        assert_eq!(err.index, 0);
        assert_eq!(err.op, "add_node");
        assert!(matches!(err.kind, GraphOpErrorKind::NodeIdExists(_)));
    }

    #[test]
    fn add_node_rejects_empty_id() {
        let err = apply_ops(
            &base(),
            &[GraphOp::AddNode {
                node: node("", NodeKind::Merge),
            }],
        )
        .unwrap_err();
        assert!(matches!(err.kind, GraphOpErrorKind::EmptyNodeId));
    }

    #[test]
    fn update_node_config_merge_patches() {
        let mut n = node("a", NodeKind::Agent);
        n.config = json!({ "prompt": "hi", "keep": 1 });
        let g0 = WorkflowGraph {
            nodes: vec![node("t", NodeKind::Trigger), n],
            ..Default::default()
        };
        let g = apply_ops(
            &g0,
            &[GraphOp::UpdateNodeConfig {
                id: "a".to_string(),
                config: json!({ "prompt": "bye", "added": true, "keep": null }),
            }],
        )
        .unwrap();
        let cfg = &g.nodes[1].config;
        assert_eq!(cfg["prompt"], "bye");
        assert_eq!(cfg["added"], true);
        assert!(
            cfg.get("keep").is_none(),
            "null leaf deletes the key: {cfg}"
        );
    }

    #[test]
    fn update_node_config_on_null_config_creates_object() {
        let g = apply_ops(
            &base(),
            &[GraphOp::UpdateNodeConfig {
                id: "a".to_string(),
                config: json!({ "x": 1 }),
            }],
        )
        .unwrap();
        assert_eq!(g.nodes[1].config["x"], 1);
    }

    #[test]
    fn update_node_config_missing_node_errors() {
        let err = apply_ops(
            &base(),
            &[GraphOp::UpdateNodeConfig {
                id: "ghost".to_string(),
                config: json!({}),
            }],
        )
        .unwrap_err();
        assert!(matches!(err.kind, GraphOpErrorKind::NodeNotFound(_)));
    }

    #[test]
    fn rename_node_rewires_edges() {
        let g = apply_ops(
            &base(),
            &[GraphOp::RenameNode {
                id: "a".to_string(),
                new_id: "agent1".to_string(),
            }],
        )
        .unwrap();
        assert!(g.nodes.iter().any(|n| n.id == "agent1"));
        assert!(g.nodes.iter().all(|n| n.id != "a"));
        assert_eq!(g.edges[0].to_node, "agent1");
    }

    #[test]
    fn rename_node_rejects_collision_and_missing() {
        let err = apply_ops(
            &base(),
            &[GraphOp::RenameNode {
                id: "a".to_string(),
                new_id: "t".to_string(),
            }],
        )
        .unwrap_err();
        assert!(matches!(err.kind, GraphOpErrorKind::NodeIdExists(_)));

        let err = apply_ops(
            &base(),
            &[GraphOp::RenameNode {
                id: "ghost".to_string(),
                new_id: "z".to_string(),
            }],
        )
        .unwrap_err();
        assert!(matches!(err.kind, GraphOpErrorKind::NodeNotFound(_)));
    }

    #[test]
    fn remove_node_drops_incident_edges() {
        let g = apply_ops(
            &base(),
            &[GraphOp::RemoveNode {
                id: "a".to_string(),
            }],
        )
        .unwrap();
        assert_eq!(g.nodes.len(), 1);
        assert!(g.edges.is_empty(), "incident edge removed");
    }

    #[test]
    fn add_edge_validates_endpoints_and_dupes() {
        // both endpoints must exist
        let err = apply_ops(
            &base(),
            &[GraphOp::AddEdge {
                edge: Edge {
                    from_node: "a".to_string(),
                    from_port: "main".to_string(),
                    to_node: "ghost".to_string(),
                    to_port: "main".to_string(),
                },
            }],
        )
        .unwrap_err();
        assert!(matches!(err.kind, GraphOpErrorKind::EdgeEndpointMissing(_)));

        // duplicate rejected
        let err = apply_ops(
            &base(),
            &[GraphOp::AddEdge {
                edge: Edge {
                    from_node: "t".to_string(),
                    from_port: "main".to_string(),
                    to_node: "a".to_string(),
                    to_port: "main".to_string(),
                },
            }],
        )
        .unwrap_err();
        assert!(matches!(err.kind, GraphOpErrorKind::EdgeExists(_)));
    }

    #[test]
    fn remove_edge_matches_or_errors() {
        let g = apply_ops(
            &base(),
            &[GraphOp::RemoveEdge {
                from_node: "t".to_string(),
                from_port: "main".to_string(),
                to_node: "a".to_string(),
                to_port: "main".to_string(),
            }],
        )
        .unwrap();
        assert!(g.edges.is_empty());

        let err = apply_ops(
            &base(),
            &[GraphOp::RemoveEdge {
                from_node: "t".to_string(),
                from_port: "main".to_string(),
                to_node: "ghost".to_string(),
                to_port: "main".to_string(),
            }],
        )
        .unwrap_err();
        assert!(matches!(err.kind, GraphOpErrorKind::EdgeNotFound(_)));
    }

    #[test]
    fn set_node_position_sets_coords() {
        let g = apply_ops(
            &base(),
            &[GraphOp::SetNodePosition {
                id: "a".to_string(),
                position: Position { x: 10.0, y: 20.0 },
            }],
        )
        .unwrap();
        assert_eq!(g.nodes[1].position, Some(Position { x: 10.0, y: 20.0 }));
    }

    #[test]
    fn ops_apply_in_sequence_and_base_is_untouched() {
        let b = base();
        let g = apply_ops(
            &b,
            &[
                GraphOp::AddNode {
                    node: node("b", NodeKind::Merge),
                },
                GraphOp::AddEdge {
                    edge: Edge {
                        from_node: "a".to_string(),
                        from_port: "main".to_string(),
                        to_node: "b".to_string(),
                        to_port: "main".to_string(),
                    },
                },
                GraphOp::RenameNode {
                    id: "b".to_string(),
                    new_id: "merge1".to_string(),
                },
            ],
        )
        .unwrap();
        assert_eq!(g.nodes.len(), 3);
        assert_eq!(g.edges.len(), 2);
        assert!(g.edges.iter().any(|e| e.to_node == "merge1"));
        // base untouched
        assert_eq!(b.nodes.len(), 2);
        assert_eq!(b.edges.len(), 1);
    }

    #[test]
    fn error_index_points_at_the_failing_op() {
        // op 0 ok, op 1 fails.
        let err = apply_ops(
            &base(),
            &[
                GraphOp::SetNodeName {
                    id: "a".to_string(),
                    name: "Renamed".to_string(),
                },
                GraphOp::RemoveNode {
                    id: "ghost".to_string(),
                },
            ],
        )
        .unwrap_err();
        assert_eq!(err.index, 1);
        assert_eq!(err.op, "remove_node");
    }

    #[test]
    fn graph_op_deserializes_from_tagged_json() {
        let op: GraphOp = serde_json::from_value(json!({
            "op": "update_node_config",
            "id": "a",
            "config": { "prompt": "hi" }
        }))
        .unwrap();
        assert!(matches!(op, GraphOp::UpdateNodeConfig { .. }));

        // remove_edge ports default to "main"
        let op: GraphOp = serde_json::from_value(json!({
            "op": "remove_edge", "from_node": "t", "to_node": "a"
        }))
        .unwrap();
        match op {
            GraphOp::RemoveEdge {
                from_port, to_port, ..
            } => {
                assert_eq!(from_port, "main");
                assert_eq!(to_port, "main");
            }
            _ => panic!("wrong variant"),
        }
    }
}
