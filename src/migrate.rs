//! Load-time migration of persisted [`WorkflowGraph`] JSON.
//!
//! A `WorkflowGraph`'s JSON is a stable, user-authored contract: definitions
//! saved by an older crate must keep loading as the model evolves. Migrations
//! are pure functions `(old_json) -> new_json`, applied on read **before**
//! deserialization, validation, and compilation:
//!
//! ```text
//! raw JSON → migrate (schema_version) → parse → validate → compile
//! ```
//!
//! The semver policy treats the JSON format as public API.
//!
//! [`WorkflowGraph`]: crate::model::WorkflowGraph

use crate::error::Result;
use crate::model::CURRENT_SCHEMA_VERSION;
use serde_json::Value;

/// Upgrades a persisted [`WorkflowGraph`] JSON value to the current schema.
///
/// The value's top-level `schema_version` is read (absent → treated as `0`) and
/// each registered schema migration is applied in order up to
/// [`CURRENT_SCHEMA_VERSION`]. There are no field-reshaping migrations yet: the
/// only step, `v0 → v1`, simply stamps the current `schema_version` onto the
/// object (older graphs predate the field). The upgraded value is returned;
/// callers then `serde_json::from_value::<WorkflowGraph>` it.
///
/// Per-node `type_version` migrations will be registered here in the same way
/// once a node kind's `config` shape changes (see the extension point below).
///
/// # Errors
///
/// Returns an error if a future migration step fails. The current no-op steps
/// never fail.
///
/// [`WorkflowGraph`]: crate::model::WorkflowGraph
pub fn migrate(mut value: Value) -> Result<Value> {
    // Absent or non-integer `schema_version` means the graph predates the field.
    let mut version = value
        .get("schema_version")
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;

    // Apply schema migrations in order, one version step at a time, until the
    // value reaches the current schema.
    //
    // Extension point: as the schema evolves, reshape `value` from `version` to
    // `version + 1` here (e.g. `match version { 1 => rename_fields(&mut value),
    // .. }`), including rewriting node `config` and per-node `type_version`. The
    // only step today, v0 → v1, is a structural no-op — the sole change is the
    // presence of the `schema_version` field itself, stamped after the loop.
    while version < CURRENT_SCHEMA_VERSION {
        version += 1;
    }

    // Stamp the resulting version so the object is self-describing on re-save.
    if let Value::Object(map) = &mut value {
        map.insert(
            "schema_version".to_string(),
            Value::from(CURRENT_SCHEMA_VERSION),
        );
    }

    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::WorkflowGraph;
    use serde_json::json;

    #[test]
    fn versionless_json_deserializes_with_defaults() {
        // A graph with no `schema_version` and a node with no `type_version`
        // still loads, with the serde defaults filled in.
        let raw = json!({
            "name": "legacy",
            "nodes": [
                { "id": "t", "kind": "trigger", "name": "Trigger" }
            ],
            "edges": []
        });

        let graph: WorkflowGraph = serde_json::from_value(raw).expect("deserialize");
        assert_eq!(graph.schema_version, CURRENT_SCHEMA_VERSION);
        assert_eq!(graph.nodes[0].type_version, 1);
    }

    #[test]
    fn migrate_stamps_current_schema_version() {
        let raw = json!({
            "name": "legacy",
            "nodes": [],
            "edges": []
        });

        let upgraded = migrate(raw).expect("migrate");
        assert_eq!(
            upgraded.get("schema_version").and_then(|v| v.as_u64()),
            Some(u64::from(CURRENT_SCHEMA_VERSION))
        );

        // And the upgraded value round-trips into the typed model.
        let graph: WorkflowGraph = serde_json::from_value(upgraded).expect("deserialize");
        assert_eq!(graph.schema_version, CURRENT_SCHEMA_VERSION);
    }

    #[test]
    fn unknown_node_kind_errors_not_panics() {
        // Forward compatibility: a node kind an older runtime doesn't know
        // surfaces as a deserialization `Err`, never a panic or silent drop.
        let raw = json!({
            "schema_version": 1,
            "nodes": [
                { "id": "x", "kind": "bogus", "name": "Mystery" }
            ],
            "edges": []
        });

        let result: std::result::Result<WorkflowGraph, _> = serde_json::from_value(raw);
        assert!(
            result.is_err(),
            "unknown node kind must fail to deserialize"
        );
    }
}
