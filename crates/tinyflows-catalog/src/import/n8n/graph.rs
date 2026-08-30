//! Trigger reconciliation and connection/port mapping onto tinyflows edges.

use serde_json::{Map, Value, json};
use tinyflows::model::{Edge, Node, NodeKind, Position};

/// Ensures the graph has exactly one trigger, mutating `nodes` in place:
/// - zero triggers → prepend a synthesized `manual` trigger (with a warning);
/// - multiple triggers → keep the first, demote the rest to placeholders.
///
/// Returns the id of the synthesized trigger, if one was added, so the caller
/// can wire it to the graph's root nodes once edges are computed.
pub(super) fn reconcile_triggers(
    nodes: &mut Vec<Node>,
    warnings: &mut Vec<String>,
) -> Option<String> {
    let trigger_idxs: Vec<usize> = nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| n.kind == NodeKind::Trigger)
        .map(|(i, _)| i)
        .collect();

    match trigger_idxs.len() {
        0 => {
            warnings.push(
                "The n8n workflow had no importable trigger — a manual trigger was added so the \
                 flow is runnable. Attach a schedule or app-event trigger to run it automatically."
                    .to_string(),
            );
            // Collision-free synthetic id: an n8n graph may already have a
            // (non-trigger) node literally named "trigger" — colliding with a
            // hardcoded id would produce a duplicate-id graph and fail
            // validation, turning an otherwise-recoverable import into a hard
            // failure.
            let mut trigger_id = "trigger".to_string();
            let mut suffix = 2;
            while nodes.iter().any(|n| n.id == trigger_id) {
                trigger_id = format!("trigger_{suffix}");
                suffix += 1;
            }

            nodes.insert(
                0,
                Node {
                    id: trigger_id.clone(),
                    kind: NodeKind::Trigger,
                    type_version: 1,
                    name: "Manual Trigger".to_string(),
                    config: json!({ "trigger_kind": "manual" }),
                    ports: Vec::new(),
                    position: None,
                },
            );
            return Some(trigger_id);
        }
        1 => {}
        _ => {
            // Keep the first trigger; demote the rest so `validate` accepts the
            // graph. Their ids are unchanged, so edges stay wired.
            for &idx in trigger_idxs.iter().skip(1) {
                let node = &mut nodes[idx];
                warnings.push(format!(
                    "The n8n workflow had more than one trigger; '{}' was imported as a \
                     placeholder because a tinyflows flow allows only one trigger.",
                    node.name
                ));
                let original = node.config.clone();
                node.kind = NodeKind::Transform;
                node.config = json!({
                    "_n8n_import": {
                        "original_type": "trigger",
                        "note": "Extra trigger demoted to a placeholder (a flow allows one trigger).",
                    },
                    "parameters": original,
                });
            }
        }
    }
    None
}

/// Rewrites n8n's name-keyed `connections` map onto tinyflows edges (id-keyed),
/// preserving output-port routing: an `if`/`condition` source routes output 0 →
/// `true` and 1 → `false`; a `switch` source routes output _i_ → `"i"`; every
/// other source uses `main`. Connections that reference an unknown node are
/// dropped with a warning.
pub(super) fn map_connections(
    connections: Option<&Value>,
    name_to_id: &Map<String, Value>,
    nodes: &[Node],
    warnings: &mut Vec<String>,
) -> Vec<Edge> {
    let mut edges = Vec::new();
    let Some(Value::Object(conns)) = connections else {
        return edges;
    };

    for (src_name, outputs) in conns {
        let Some(src_id) = name_to_id.get(src_name).and_then(Value::as_str) else {
            continue;
        };
        let src_kind = nodes
            .iter()
            .find(|n| n.id == src_id)
            .map(|n| n.kind.clone());
        // n8n groups outputs by connection type (`main`, `ai_tool`, …); we only
        // wire `main` — other connection families have no tinyflows analogue.
        let Some(main) = outputs.get("main").and_then(Value::as_array) else {
            continue;
        };
        for (port_index, port_targets) in main.iter().enumerate() {
            let from_port = output_port_name(src_kind.as_ref(), port_index);
            let Some(targets) = port_targets.as_array() else {
                continue;
            };
            for target in targets {
                let Some(tgt_name) = target.get("node").and_then(Value::as_str) else {
                    continue;
                };
                match name_to_id.get(tgt_name).and_then(Value::as_str) {
                    Some(tgt_id) => edges.push(Edge {
                        from_node: src_id.to_string(),
                        from_port: from_port.clone(),
                        to_node: tgt_id.to_string(),
                        to_port: "main".to_string(),
                    }),
                    None => warnings.push(format!(
                        "Connection from '{src_name}' to unknown node '{tgt_name}' was dropped."
                    )),
                }
            }
        }
    }
    edges
}

/// The tinyflows output-port name for source `kind`'s n8n output index.
pub(super) fn output_port_name(kind: Option<&NodeKind>, index: usize) -> String {
    match kind {
        Some(NodeKind::Condition) => {
            if index == 0 {
                "true".to_string()
            } else {
                "false".to_string()
            }
        }
        Some(NodeKind::Switch) => index.to_string(),
        _ => {
            if index == 0 {
                "main".to_string()
            } else {
                index.to_string()
            }
        }
    }
}

/// Parses n8n's `position: [x, y]` array into a tinyflows [`Position`].
pub(super) fn parse_position(value: Option<&Value>) -> Option<Position> {
    let arr = value?.as_array()?;
    let x = arr.first()?.as_f64()?;
    let y = arr.get(1)?.as_f64()?;
    Some(Position { x, y })
}

/// Derives a stable, id-safe slug from an n8n node name when the node carries
/// no `id` of its own.
pub(super) fn slug(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() { "node".to_string() } else { s }
}
