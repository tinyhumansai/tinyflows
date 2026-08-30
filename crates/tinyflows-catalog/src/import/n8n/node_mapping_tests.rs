//! Per-node-type mapping, including the kinds with no tinyflows equivalent.
//!
//! An unmapped type is not a failed import: it lands as an annotated placeholder
//! carrying the original payload, so the graph still loads and validates.

use super::*;

#[test]
fn maps_if_node_to_condition_with_true_false_ports() {
    let wf = json!({
        "name": "branch",
        "nodes": [
            { "id": "s", "name": "Schedule Trigger", "type": "n8n-nodes-base.scheduleTrigger", "position": [0, 0] },
            { "id": "c", "name": "IF", "type": "n8n-nodes-base.if", "position": [200, 0] },
            { "id": "a", "name": "Yes", "type": "n8n-nodes-base.httpRequest", "position": [400, -100] },
            { "id": "b", "name": "No", "type": "n8n-nodes-base.httpRequest", "position": [400, 100] }
        ],
        "connections": {
            "Schedule Trigger": { "main": [[{ "node": "IF", "type": "main", "index": 0 }]] },
            "IF": { "main": [
                [{ "node": "Yes", "type": "main", "index": 0 }],
                [{ "node": "No", "type": "main", "index": 0 }]
            ] }
        }
    });
    let result = map_n8n_workflow(&wf).expect("map");
    let g = &result.graph;
    assert_eq!(g.name, "branch");

    let cond = g.node("c").expect("condition node");
    assert_eq!(cond.kind, NodeKind::Condition);

    let trig = g.node("s").expect("trigger node");
    assert_eq!(trig.kind, NodeKind::Trigger);
    assert_eq!(trig.config["trigger_kind"], json!("schedule"));
    assert_eq!(trig.position, Some(Position { x: 0.0, y: 0.0 }));

    // The IF node's two outputs route to `true`/`false` ports.
    let true_edge = g
        .edges
        .iter()
        .find(|e| e.from_node == "c" && e.to_node == "a")
        .expect("true edge");
    assert_eq!(true_edge.from_port, "true");
    let false_edge = g
        .edges
        .iter()
        .find(|e| e.from_node == "c" && e.to_node == "b")
        .expect("false edge");
    assert_eq!(false_edge.from_port, "false");

    // Whole graph is structurally valid (exactly one trigger, real edges).
    tinyflows::validate::validate(g).expect("valid graph");
}

#[test]
fn unmapped_type_becomes_annotated_placeholder_not_a_failure() {
    let wf = json!({
        "name": "exotic",
        "nodes": [
            { "id": "t", "name": "Manual", "type": "n8n-nodes-base.manualTrigger" },
            { "id": "x", "name": "Airtable", "type": "n8n-nodes-base.airtable",
              "parameters": { "operation": "append", "table": "leads" } }
        ],
        "connections": {
            "Manual": { "main": [[{ "node": "Airtable", "type": "main", "index": 0 }]] }
        }
    });
    let result = map_n8n_workflow(&wf).expect("map");
    let node = result.graph.node("x").expect("placeholder node");
    assert_eq!(node.kind, NodeKind::Transform);
    assert_eq!(
        node.config["_n8n_import"]["original_type"],
        json!("n8n-nodes-base.airtable")
    );
    // Original parameters are preserved for editing.
    assert_eq!(node.config["parameters"]["table"], json!("leads"));
    // The unmapped type produced a warning.
    assert!(result.warnings.iter().any(|w| w.contains("airtable")));
    tinyflows::validate::validate(&result.graph).expect("valid graph");
}

#[test]
fn http_request_maps_url_and_method() {
    let mut warnings = Vec::new();
    let cfg = map_http_request(
        &json!({ "url": "https://api.example.com", "requestMethod": "POST" }),
        &mut warnings,
        "HTTP",
    );
    assert_eq!(cfg["url"], json!("https://api.example.com"));
    assert_eq!(cfg["method"], json!("POST"));
    // Expression in the url is translated in place.
    let cfg2 = map_http_request(
        &json!({ "url": "={{ $json.endpoint }}" }),
        &mut warnings,
        "HTTP",
    );
    assert_eq!(cfg2["url"], json!("=.item.endpoint"));
    assert_eq!(cfg2["method"], json!("GET"));
}

#[test]
fn http_request_normalizes_json_body_named_body_fields_and_headers() {
    let mut warnings = Vec::new();
    let cfg = map_http_request(
        &json!({
            "method": "POST",
            "bodyParameters": { "parameters": [
                { "name": "subject", "value": "hello" },
                { "name": "count", "value": 2 }
            ] },
            "headerParameters": { "parameters": [
                { "name": "X-Trace", "value": "abc" }
            ] }
        }),
        &mut warnings,
        "HTTP",
    );
    assert_eq!(cfg["body"], json!({ "subject": "hello", "count": 2 }));
    assert_eq!(cfg["headers"], json!({ "X-Trace": "abc" }));
    assert!(warnings.is_empty(), "{warnings:?}");

    let cfg = map_http_request(
        &json!({ "jsonBody": { "ready": true } }),
        &mut warnings,
        "JSON HTTP",
    );
    assert_eq!(cfg["body"], json!({ "ready": true }));
}

#[test]
fn code_node_pulls_source_and_language() {
    let mut warnings = Vec::new();
    let cfg = map_code(&json!({ "jsCode": "return items;" }), &mut warnings, "Code");
    assert_eq!(cfg["source"], json!("return items;"));
    assert_eq!(cfg["language"], json!("javascript"));
}

#[test]
fn switch_ports_are_numeric_indices() {
    assert_eq!(output_port_name(Some(&NodeKind::Switch), 0), "0");
    assert_eq!(output_port_name(Some(&NodeKind::Switch), 2), "2");
    assert_eq!(output_port_name(Some(&NodeKind::Condition), 0), "true");
    assert_eq!(output_port_name(Some(&NodeKind::Condition), 1), "false");
    assert_eq!(output_port_name(Some(&NodeKind::Merge), 0), "main");
}

#[test]
fn if_node_with_untranslatable_conditions_warns_and_preserves_them() {
    let mut warnings = Vec::new();
    let cfg = map_condition(
        &json!({
            "conditions": {
                "options": {},
                "conditions": [{ "leftValue": "={{ $json.status }}", "rightValue": "ok", "operator": { "operation": "equals" } }],
            }
        }),
        &mut warnings,
        "IF",
    );
    // No `field` could be derived, so the node would otherwise silently
    // route every input the same way; the original conditions are kept for
    // the author to rebuild from, and a warning is raised.
    assert!(cfg.get("field").is_none());
    assert!(cfg["_n8n_import"]["conditions"].is_object());
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("IF") && w.contains("conditions"))
    );
}

#[test]
fn switch_node_with_untranslatable_rules_warns_and_preserves_them() {
    let mut warnings = Vec::new();
    let cfg = map_switch(
        &json!({ "rules": { "values": [{ "conditions": {}, "outputKey": "a" }] } }),
        &mut warnings,
        "Switch",
    );
    assert!(cfg.get("field").is_none());
    assert!(cfg.get("expression").is_none());
    assert!(cfg["_n8n_import"]["rules"].is_object());
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("Switch") && w.contains("rules"))
    );
}

#[test]
fn split_out_maps_field_to_split_out_to_path() {
    let mut warnings = Vec::new();
    let cfg = map_split_out(
        &json!({ "fieldToSplitOut": "data.items" }),
        &mut warnings,
        "Split",
    );
    assert_eq!(cfg["path"], json!("data.items"));
    assert!(cfg.get("fieldToSplitOut").is_none());
}

#[test]
fn item_lists_only_maps_to_split_out_for_the_split_out_operation() {
    let wf = json!({
        "name": "item-lists",
        "nodes": [
            { "id": "t", "name": "Manual", "type": "n8n-nodes-base.manualTrigger" },
            { "id": "s", "name": "Split", "type": "n8n-nodes-base.itemLists",
              "parameters": { "operation": "splitOutItems", "fieldToSplitOut": "items" } },
            { "id": "a", "name": "Aggregate", "type": "n8n-nodes-base.itemLists",
              "parameters": { "operation": "aggregateItems" } }
        ],
        "connections": {
            "Manual": { "main": [[{ "node": "Split", "type": "main", "index": 0 }]] },
            "Split": { "main": [[{ "node": "Aggregate", "type": "main", "index": 0 }]] }
        }
    });
    let result = map_n8n_workflow(&wf).expect("map");
    assert_eq!(
        result.graph.node("s").expect("split node").kind,
        NodeKind::SplitOut
    );
    // A non-split-out operation is not force-mapped to `split_out`; it falls
    // through to the unmapped-type placeholder like any other unrecognized
    // config, rather than silently claiming to aggregate.
    assert_eq!(
        result.graph.node("a").expect("aggregate node").kind,
        NodeKind::Transform
    );
}

#[test]
fn code_node_with_n8n_globals_or_top_level_return_warns() {
    let mut warnings = Vec::new();
    map_code(&json!({ "jsCode": "return items;" }), &mut warnings, "Code");
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("Code") && w.contains("n8n-only globals"))
    );

    let mut clean_warnings = Vec::new();
    map_code(
        &json!({ "jsCode": "const x = 1; module.exports = x;" }),
        &mut clean_warnings,
        "Clean",
    );
    assert!(
        !clean_warnings
            .iter()
            .any(|w| w.contains("n8n-only globals")),
        "code with no n8n tell-tales should not warn: {clean_warnings:?}"
    );
}

#[test]
fn incompatible_n8n_code_is_a_placeholder_not_an_executable_code_node() {
    let mut warnings = Vec::new();
    let (kind, cfg) = map_code_node(&json!({ "jsCode": "return items;" }), &mut warnings, "Code");
    assert_eq!(kind, NodeKind::Transform);
    assert_eq!(cfg["_n8n_import"]["original_type"], json!("code"));

    let (kind, _) = map_code_node(
        &json!({ "jsCode": "process.stdin.pipe(process.stdout);" }),
        &mut Vec::new(),
        "Portable",
    );
    assert_eq!(kind, NodeKind::Code);
}

#[test]
fn cron_node_maps_cron_expression_to_schedule() {
    let mut warnings = Vec::new();
    let cfg = trigger_config(
        "schedule",
        &json!({ "cronExpression": "0 9 * * *" }),
        &mut warnings,
        "Cron",
    );
    assert_eq!(
        cfg["schedule"],
        json!({ "kind": "cron", "expr": "0 9 * * *" })
    );
    assert!(
        !warnings
            .iter()
            .any(|w| w.contains("could not be translated"))
    );
}

#[test]
fn interval_node_maps_unit_and_value_to_every_ms() {
    let mut warnings = Vec::new();
    let cfg = trigger_config(
        "schedule",
        &json!({ "unit": "minutes", "value": 15 }),
        &mut warnings,
        "Interval",
    );
    assert_eq!(
        cfg["schedule"],
        json!({ "kind": "every", "every_ms": 900000.0 })
    );
    assert!(
        !warnings
            .iter()
            .any(|w| w.contains("could not be translated"))
    );
}

#[test]
fn schedule_trigger_maps_a_cron_expression_rule() {
    let mut warnings = Vec::new();
    let cfg = trigger_config(
        "schedule",
        &json!({
            "rule": { "interval": [{ "field": "cronExpression", "expression": "*/5 * * * *" }] }
        }),
        &mut warnings,
        "ScheduleTrigger",
    );
    assert_eq!(
        cfg["schedule"],
        json!({ "kind": "cron", "expr": "*/5 * * * *" })
    );
    assert!(
        !warnings
            .iter()
            .any(|w| w.contains("could not be translated"))
    );
}

#[test]
fn schedule_trigger_maps_a_fixed_unit_interval_rule() {
    let mut warnings = Vec::new();
    let cfg = trigger_config(
        "schedule",
        &json!({
            "rule": { "interval": [{ "field": "hours", "hoursInterval": 2 }] }
        }),
        &mut warnings,
        "ScheduleTrigger",
    );
    assert_eq!(
        cfg["schedule"],
        json!({ "kind": "every", "every_ms": 7200000.0 })
    );
}

#[test]
fn unrecognized_schedule_shape_warns_instead_of_guessing() {
    let mut warnings = Vec::new();
    let cfg = trigger_config(
        "schedule",
        &json!({ "rule": { "interval": [{ "field": "weekday", "weekday": 1 }] } }),
        &mut warnings,
        "Weekly",
    );
    assert!(cfg.get("schedule").is_none());
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("Weekly") && w.contains("could not be translated"))
    );
}

#[test]
fn multiple_schedule_intervals_warn_instead_of_dropping_cadences() {
    let mut warnings = Vec::new();
    let cfg = trigger_config(
        "schedule",
        &json!({ "rule": { "interval": [
            { "field": "hours", "hoursInterval": 2 },
            { "field": "hours", "hoursInterval": 6 }
        ] } }),
        &mut warnings,
        "Several cadences",
    );
    assert!(cfg.get("schedule").is_none());
    assert!(warnings.iter().any(|warning| {
        warning.contains("Several cadences") && warning.contains("could not be translated")
    }));
}
