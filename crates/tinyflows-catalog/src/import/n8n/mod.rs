//! Best-effort importer that maps an **n8n** workflow export into a tinyflows
//! [`WorkflowGraph`]. Backs the `format: "n8n"` branch of `flows_import`
//! (`schemas::handle_import` → `ops::flows_import`).
//!
//! n8n and tinyflows share a large slice of automation vocabulary (branching,
//! merging, HTTP, code, triggers), so this maps the overlap directly:
//!
//! | n8n node type (`n8n-nodes-base.*`)      | tinyflows kind          |
//! | --------------------------------------- | ----------------------- |
//! | `if`                                    | `condition`             |
//! | `switch`                                | `switch`                |
//! | `merge`                                 | `merge`                 |
//! | `splitOut` / `itemLists`(splitOut mode) | `split_out`             |
//! | `httpRequest`                           | `http_request`          |
//! | `code` / `function` / `functionItem`    | `code`                  |
//! | `scheduleTrigger` / `cron` / `interval` | `trigger` (schedule)    |
//! | `webhook`                               | `trigger` (webhook)     |
//! | `manualTrigger`                         | `trigger` (manual)      |
//!
//! **Everything else is not a failed import** — an unmapped node type lands as
//! an annotated placeholder (`transform`) node carrying the original n8n type
//! and parameters in its `config`, plus a `_n8n_import` note, so the graph
//! still loads, validates, and can be edited on the canvas. Connections and
//! canvas positions are preserved wherever the source provides them.
//!
//! The mapping is intentionally lossy and advisory: every approximation
//! (unmapped type, untranslated expression, synthesized/demoted trigger) is
//! reported as a warning string the UI surfaces next to the imported draft.

use serde_json::{Map, Value};
use tinyflows::model::{Edge, Node, WorkflowGraph};

/// The outcome of mapping an n8n workflow: the best-effort tinyflows graph plus
/// the list of advisory warnings collected during the mapping.
#[derive(Debug)]
pub struct N8nImportResult {
    /// The mapped graph (still passed through `migrate` + `validate` by the
    /// caller before it is handed to the UI).
    pub graph: WorkflowGraph,
    /// Human-readable, non-fatal notes: unmapped node types, untranslated
    /// expressions, and any synthesized/demoted trigger.
    pub warnings: Vec<String>,
}

/// Returns `true` when `value` looks like an n8n workflow export rather than a
/// native tinyflows `WorkflowGraph` — used by `flows_import`'s auto-detect. The
/// tell-tales are a top-level `connections` object and/or nodes carrying an
/// `n8n-nodes-base.*`/`type`-style discriminator (tinyflows nodes use `kind`).
pub fn looks_like_n8n(value: &Value) -> bool {
    if value.get("connections").map(Value::is_object) == Some(true) {
        return true;
    }
    let Some(nodes) = value.get("nodes").and_then(Value::as_array) else {
        return false;
    };
    nodes.iter().any(|n| {
        // A native tinyflows node has `kind`; an n8n node has `type` and no `kind`.
        n.get("kind").is_none() && n.get("type").and_then(Value::as_str).is_some()
    })
}

/// Maps a parsed n8n workflow JSON `value` into a tinyflows [`WorkflowGraph`].
///
/// Never returns `Err` for an unrecognized node type — those become annotated
/// placeholders. `Err` is reserved for input that is not shaped like an n8n
/// export at all (e.g. `nodes` is not an array).
pub fn map_n8n_workflow(value: &Value) -> Result<N8nImportResult, String> {
    let mut warnings: Vec<String> = Vec::new();

    let name = value
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("Imported workflow")
        .to_string();

    let raw_nodes = value
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| "n8n workflow is missing a `nodes` array".to_string())?;

    tracing::debug!(
        target: "flows",
        %name,
        node_count = raw_nodes.len(),
        "[flows] n8n_import: mapping n8n workflow"
    );

    // n8n connections key nodes by *name*; tinyflows edges reference node *ids*.
    // Build a name → id lookup so connections can be rewired onto ids.
    let mut name_to_id: Map<String, Value> = Map::new();
    let mut nodes: Vec<Node> = Vec::new();

    for raw in raw_nodes {
        let n8n_name = raw
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("node")
            .to_string();
        let id = raw
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| slug(&n8n_name));
        // n8n's `connections` map is keyed by node NAME (not id), and
        // `map_connections` below rewires every edge that names this node
        // through this lookup. A duplicate name is last-wins here, which
        // silently mis-wires every connection naming it onto whichever node
        // happened to be inserted last — R-m6. Every other approximation in
        // this importer reports a warning; a duplicate name is the one
        // silent mis-wiring, so warn on the collision the same way.
        if let Some(previous_id) = name_to_id
            .insert(n8n_name.clone(), Value::String(id.clone()))
            .and_then(|v| v.as_str().map(str::to_string))
        {
            tracing::warn!(
                target: "flows",
                name = %n8n_name,
                previous_id = %previous_id,
                new_id = %id,
                "[flows] n8n_import: duplicate node name collision — connections will mis-wire"
            );
            warnings.push(format!(
                "Multiple n8n nodes are named '{n8n_name}' (ids '{previous_id}' and '{id}') — \
                 n8n connections are keyed by node name, so every connection naming \
                 '{n8n_name}' was rewired onto node '{id}' and may now point at the wrong \
                 node. Rename the duplicates in the source n8n workflow and re-import, or fix \
                 the affected edges by hand."
            ));
        }

        let n8n_type = raw.get("type").and_then(Value::as_str).unwrap_or("");
        let params = raw.get("parameters").cloned().unwrap_or(Value::Null);
        let position = parse_position(raw.get("position"));

        let (kind, config) = map_node(n8n_type, &params, &n8n_name, &mut warnings);
        nodes.push(Node {
            id,
            kind,
            type_version: 1,
            name: n8n_name,
            config,
            ports: Vec::new(),
            position,
        });
    }

    // tinyflows requires exactly one trigger. Reconcile the mapped triggers:
    // synthesize a manual one when none survived, or demote extras to
    // placeholders when several did — either way `validate` will pass.
    let synthesized_trigger_id = reconcile_triggers(&mut nodes, &mut warnings);

    let mut edges = map_connections(value.get("connections"), &name_to_id, &nodes, &mut warnings);

    // A synthesized trigger starts with no outgoing edges — nothing in the
    // source export names it, since it never existed there. Without wiring it
    // to the graph's actual entry points, the flow "validates" but running it
    // executes only the disconnected trigger and none of the imported
    // workflow. Wire it to every node that has no incoming edge of its own
    // (the graph's roots), excluding the trigger itself.
    if let Some(trigger_id) = synthesized_trigger_id {
        let has_incoming: std::collections::HashSet<String> =
            edges.iter().map(|e| e.to_node.clone()).collect();
        let root_ids: Vec<String> = nodes
            .iter()
            .filter(|n| n.id != trigger_id && !has_incoming.contains(&n.id))
            .map(|n| n.id.clone())
            .collect();
        for root_id in root_ids {
            edges.push(Edge {
                from_node: trigger_id.clone(),
                from_port: "main".to_string(),
                to_node: root_id,
                to_port: "main".to_string(),
            });
        }
    }

    let graph = WorkflowGraph {
        schema_version: tinyflows::model::CURRENT_SCHEMA_VERSION,
        id: None,
        name,
        // n8n has no equivalent of a declared workflow input — its workflows are
        // parameterized through trigger/node config — so an import declares
        // none. The author adds them afterwards if the flow needs them.
        inputs: Vec::new(),
        // n8n agent nodes carry their configuration inline; they do not define
        // reusable TinyFlows agent-registry entries, so every `agent_ref`
        // resolves against this host's own registry instead.
        agents: Vec::new(),
        nodes,
        edges,
    };

    tracing::debug!(
        target: "flows",
        node_count = graph.nodes.len(),
        edge_count = graph.edges.len(),
        warning_count = warnings.len(),
        "[flows] n8n_import: mapping complete"
    );

    Ok(N8nImportResult { graph, warnings })
}

mod expr;
mod graph;
mod node_mapping;

use graph::{map_connections, parse_position, reconcile_triggers, slug};
use node_mapping::map_node;

#[cfg(test)]
use expr::{jq_field, translate_expr};
#[cfg(test)]
use graph::output_port_name;
#[cfg(test)]
use node_mapping::trigger_config;
#[cfg(test)]
use node_mapping::{
    map_code, map_code_node, map_condition, map_http_request, map_http_request_node, map_split_out,
    map_switch,
};
#[cfg(test)]
use serde_json::json;
#[cfg(test)]
use tinyflows::model::{NodeKind, Position};

// Split along the same seams as the production modules, so a reader finds the
// test beside the thing it tests.
#[cfg(test)]
#[path = "expr_tests.rs"]
mod expr_tests;
#[cfg(test)]
#[path = "graph_tests.rs"]
mod graph_tests;
#[cfg(test)]
#[path = "node_mapping_tests.rs"]
mod node_mapping_tests;
