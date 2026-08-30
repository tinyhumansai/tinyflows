//! Graph-level import: triggers, connections, and what counts as an n8n export.
//!
//! An n8n workflow can carry no trigger, several, or edges naming nodes that are
//! not there. None of those may fail the import — each becomes a warning and a
//! graph the author can still open.

use super::*;

#[test]
fn looks_like_n8n_detects_connections_and_typed_nodes() {
    assert!(looks_like_n8n(&json!({ "connections": {} })));
    assert!(looks_like_n8n(&json!({
        "nodes": [{ "name": "x", "type": "n8n-nodes-base.httpRequest" }]
    })));
    // A native tinyflows graph is not mistaken for n8n.
    assert!(!looks_like_n8n(&json!({
        "nodes": [{ "id": "t", "kind": "trigger", "name": "start" }],
        "edges": []
    })));
}

#[test]
fn synthesizes_manual_trigger_when_none_present() {
    let wf = json!({
        "name": "no-trigger",
        "nodes": [
            { "id": "h", "name": "HTTP", "type": "n8n-nodes-base.httpRequest" }
        ],
        "connections": {}
    });
    let result = map_n8n_workflow(&wf).expect("map");
    let trigger = result
        .graph
        .nodes
        .iter()
        .find(|n| n.kind == NodeKind::Trigger)
        .expect("a trigger was synthesized");
    assert!(result.warnings.iter().any(|w| w.contains("manual trigger")));
    tinyflows::validate::validate(&result.graph).expect("valid graph");

    // The synthesized trigger must be wired to the graph's actual entry
    // point — otherwise the flow validates but running it executes only the
    // disconnected trigger and none of the imported workflow.
    assert!(
        result
            .graph
            .edges
            .iter()
            .any(|e| e.from_node == trigger.id && e.to_node == "h"),
        "synthesized trigger must connect to the imported root node, got edges: {:?}",
        result.graph.edges
    );
}

#[test]
fn synthesized_trigger_id_avoids_colliding_with_an_existing_node() {
    // The n8n graph already has a (non-trigger) node literally id'd
    // "trigger" — the synthesized manual trigger must not collide with it.
    let wf = json!({
        "name": "id-collision",
        "nodes": [
            { "id": "trigger", "name": "HTTP", "type": "n8n-nodes-base.httpRequest" }
        ],
        "connections": {}
    });
    let result = map_n8n_workflow(&wf).expect("map");
    let ids: Vec<&str> = result.graph.nodes.iter().map(|n| n.id.as_str()).collect();
    // Both the original node and the synthesized trigger survive, under
    // distinct ids.
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&"trigger"));
    assert!(ids.iter().any(|id| *id != "trigger"));
    assert_eq!(
        result
            .graph
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Trigger)
            .count(),
        1
    );
    tinyflows::validate::validate(&result.graph).expect("valid graph");
}

#[test]
fn demotes_extra_triggers_to_placeholders() {
    let wf = json!({
        "name": "two-triggers",
        "nodes": [
            { "id": "s", "name": "Schedule", "type": "n8n-nodes-base.scheduleTrigger" },
            { "id": "w", "name": "Webhook", "type": "n8n-nodes-base.webhook" }
        ],
        "connections": {}
    });
    let result = map_n8n_workflow(&wf).expect("map");
    assert_eq!(
        result
            .graph
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Trigger)
            .count(),
        1
    );
    // The demoted trigger is now a placeholder transform.
    let demoted = result.graph.node("w").expect("webhook node");
    assert_eq!(demoted.kind, NodeKind::Transform);
    tinyflows::validate::validate(&result.graph).expect("valid graph");
}

#[test]
fn missing_nodes_array_is_an_error() {
    let err = map_n8n_workflow(&json!({ "name": "x" })).unwrap_err();
    assert!(err.contains("nodes"));
}

#[test]
fn drops_connection_to_unknown_node_with_warning() {
    let wf = json!({
        "name": "dangling",
        "nodes": [
            { "id": "t", "name": "Manual", "type": "n8n-nodes-base.manualTrigger" }
        ],
        "connections": {
            "Manual": { "main": [[{ "node": "Ghost", "type": "main", "index": 0 }]] }
        }
    });
    let result = map_n8n_workflow(&wf).expect("map");
    assert!(result.graph.edges.is_empty());
    assert!(result.warnings.iter().any(|w| w.contains("Ghost")));
}

// ── R-m6: duplicate n8n node names ──────────────────────────────────────

#[test]
fn duplicate_node_name_collision_emits_a_warning() {
    // n8n's `connections` map is keyed by node NAME, not id. Two nodes
    // sharing the name "HTTP" mean `name_to_id` last-wins onto id "b" —
    // any connection naming "HTTP" (including the trigger's own edge)
    // silently rewires onto "b" instead of "a" with no warning, unless
    // this collision is reported (R-m6: every other approximation in
    // this importer warns; this was the one silent mis-wiring).
    let wf = json!({
        "name": "dup-names",
        "nodes": [
            { "id": "t", "name": "Manual", "type": "n8n-nodes-base.manualTrigger" },
            { "id": "a", "name": "HTTP", "type": "n8n-nodes-base.httpRequest",
              "parameters": { "url": "https://a.example.com" } },
            { "id": "b", "name": "HTTP", "type": "n8n-nodes-base.httpRequest",
              "parameters": { "url": "https://b.example.com" } }
        ],
        "connections": {
            "Manual": { "main": [[{ "node": "HTTP", "type": "main", "index": 0 }]] }
        }
    });
    let result = map_n8n_workflow(&wf).expect("map");

    // The collision itself is reported.
    let collision_warning = result
        .warnings
        .iter()
        .find(|w| w.contains("named 'HTTP'"))
        .unwrap_or_else(|| {
            panic!(
                "expected a duplicate-name warning, got {:?}",
                result.warnings
            )
        });
    assert!(collision_warning.contains('a'));
    assert!(collision_warning.contains('b'));

    // Both original nodes still exist under their own ids — nothing was
    // dropped, only the *name*-keyed connection lookup collided.
    assert!(result.graph.node("a").is_some());
    assert!(result.graph.node("b").is_some());

    // The connection resolves deterministically onto exactly one target
    // (last-wins onto "b") rather than silently vanishing or duplicating
    // — the fix is the warning, not a change to which id wins.
    assert_eq!(result.graph.edges.len(), 1);
    assert_eq!(result.graph.edges[0].to_node, "b");

    tinyflows::validate::validate(&result.graph).expect("valid graph");
}

// ── R-C1 end-to-end: n8n `$json` import passes binding-resolvability ───

// ── Node-mapping fidelity: warn instead of silently mis-executing ──────────
