#[test]
fn untranslated_json_body_expression_makes_the_http_node_a_placeholder() {
    let mut warnings = Vec::new();
    let (kind, cfg) = map_http_request_node(
        &json!({ "jsonBody": "={{ $json.payload + 1 }}" }),
        &mut warnings,
        "Expression HTTP",
    );
    assert_eq!(kind, NodeKind::Transform);
    assert_eq!(
        cfg["_n8n_import"]["untranslated"]["jsonBody"],
        json!("={{ $json.payload + 1 }}")
    );
}

#[test]
fn unsupported_http_parts_are_all_preserved_for_repair() {
    let mut warnings = Vec::new();
    let (_, cfg) = map_http_request_node(
        &json!({
            "bodyParameters": { "unsupported": "body" },
            "headerParameters": { "unsupported": "headers" }
        }),
        &mut warnings,
        "Broken HTTP",
    );
    assert_eq!(
        cfg["_n8n_import"]["untranslated"]["bodyParameters"],
        json!({ "unsupported": "body" })
    );
    assert_eq!(
        cfg["_n8n_import"]["untranslated"]["headerParameters"],
        json!({ "unsupported": "headers" })
    );
}

#[test]
fn expressions_inside_serialized_json_bodies_are_translated_or_preserved_for_repair() {
    let (kind, cfg) = map_http_request_node(
        &json!({ "jsonBody": r#"{"payload":"={{ $json.payload }}"}"# }),
        &mut Vec::new(),
        "Nested expression HTTP",
    );
    assert_eq!(kind, NodeKind::HttpRequest);
    assert_eq!(cfg["body"]["payload"], json!("=.item.payload"));

    let (kind, cfg) = map_http_request_node(
        &json!({ "jsonBody": r#"{"payload":"={{ $json.payload + 1 }}"}"# }),
        &mut Vec::new(),
        "Untranslated nested expression HTTP",
    );
    assert_eq!(kind, NodeKind::Transform);
    assert_eq!(
        cfg["_n8n_import"]["untranslated"]["jsonBody"]["payload"],
        json!("={{ $json.payload + 1 }}")
    );
}
