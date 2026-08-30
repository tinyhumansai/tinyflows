#[test]
fn an_indexed_missing_envelope_binding_is_still_rejected() {
    let graph = graph(json!([
        { "id": "fetch", "kind": "agent", "name": "Fetch", "config": {} },
        { "id": "notify", "kind": "tool_call", "name": "Notify",
          "config": { "slug": "demo:echo",
                      "args": { "text": "=nodes.fetch.item.results[0].title" } } },
    ]));

    let failures = failures(&graph);
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(failures[0].contains("results[0].title"), "{failures:?}");
}

#[test]
fn an_indexed_array_inside_the_envelope_is_accepted() {
    let graph = graph(json!([
        { "id": "fetch", "kind": "agent", "name": "Fetch", "config": {
            "output_parser": { "schema": {
                "type": "array",
                "items": { "type": "object", "properties": {
                    "title": { "type": "string" }
                } }
            } }
        } },
        { "id": "notify", "kind": "tool_call", "name": "Notify",
          "config": { "slug": "demo:echo",
                      "args": { "text": "=nodes.fetch.item.json[0].title" } } },
    ]));

    assert!(failures(&graph).is_empty(), "{:?}", failures(&graph));
    assert_eq!(
        parse_node_binding("=nodes.fetch.item.json[0].title"),
        Some(crate::bindings::NodeBinding {
            node_id: "fetch".to_string(),
            through_envelope: true,
            field_path: "[0].title".to_string(),
        })
    );
}
