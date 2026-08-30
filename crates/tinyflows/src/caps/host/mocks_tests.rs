//! Tests for the schema-aware dry-run stand-ins.

use serde_json::json;

use super::*;

#[test]
fn a_dry_run_sample_satisfies_the_shape_a_node_declared() {
    let sample = sample_for_schema(&json!({
        "type": "object",
        "properties": {
            "title": { "type": "string" },
            "count": { "type": "integer" },
            "tags":  { "type": "array", "items": { "type": "string" } },
            "state": { "enum": ["open", "closed"] }
        }
    }));

    assert!(sample["title"].is_string());
    assert!(sample["count"].is_number());
    // One element, so a downstream per-item node has something to map over.
    assert_eq!(sample["tags"].as_array().unwrap().len(), 1);
    assert_eq!(sample["state"], "open");
}
