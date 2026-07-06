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

use crate::error::{Result, ValidationError};
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
/// # Examples
///
/// ```
/// use serde_json::json;
/// use tinyflows::migrate::migrate;
///
/// // A versionless graph gains the current `schema_version` on load.
/// let upgraded = migrate(json!({
///     "name": "legacy",
///     "nodes": [],
///     "edges": []
/// }))
/// .unwrap();
/// assert_eq!(upgraded["schema_version"], json!(1));
///
/// // An already-current document is returned unchanged in value.
/// let current = json!({ "schema_version": 1, "name": "ok", "nodes": [], "edges": [] });
/// assert_eq!(migrate(current.clone()).unwrap(), current);
/// ```
///
/// # Errors
///
/// Returns [`ValidationError::SchemaVersionTooNew`] if the document declares a
/// `schema_version` greater than [`CURRENT_SCHEMA_VERSION`] — such a graph
/// cannot be safely migrated and must never be silently downgraded. Also
/// returns an error if a future migration step fails; the current no-op steps
/// never fail.
///
/// [`ValidationError::SchemaVersionTooNew`]: crate::error::ValidationError::SchemaVersionTooNew
///
/// [`WorkflowGraph`]: crate::model::WorkflowGraph
pub fn migrate(mut value: Value) -> Result<Value> {
    // Absent or non-integer `schema_version` means the graph predates the field.
    let mut version = value
        .get("schema_version")
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;

    // A document newer than this crate understands must NOT be silently
    // downgraded (rewriting its `schema_version` down would corrupt it). Refuse
    // to migrate it and leave the value untouched — the caller should upgrade
    // the crate to load such a graph.
    if version > CURRENT_SCHEMA_VERSION {
        return Err(ValidationError::SchemaVersionTooNew {
            found: version,
            supported: CURRENT_SCHEMA_VERSION,
        }
        .into());
    }

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

    #[test]
    fn versionless_graph_gains_schema_and_node_type_version_defaults() {
        // A graph with neither `schema_version` nor node `type_version` set.
        let raw = json!({
            "name": "legacy",
            "nodes": [
                { "id": "t", "kind": "trigger", "name": "Trigger" },
                { "id": "a", "kind": "agent", "name": "Agent" }
            ],
            "edges": []
        });

        let upgraded = migrate(raw).expect("migrate");
        // `schema_version` is stamped at the top level by migrate itself.
        assert_eq!(upgraded["schema_version"], json!(CURRENT_SCHEMA_VERSION));

        // Node `type_version` is filled by the serde default on deserialize.
        let graph: WorkflowGraph = serde_json::from_value(upgraded).expect("deserialize");
        assert_eq!(graph.schema_version, CURRENT_SCHEMA_VERSION);
        assert!(graph.nodes.iter().all(|n| n.type_version == 1));
    }

    #[test]
    fn already_current_document_passes_through_unchanged() {
        let current = json!({
            "schema_version": CURRENT_SCHEMA_VERSION,
            "name": "ok",
            "nodes": [
                { "id": "t", "kind": "trigger", "type_version": 1, "name": "Trigger" }
            ],
            "edges": []
        });

        let out = migrate(current.clone()).expect("migrate");
        // Value is byte-for-byte identical: no fields added, removed, or changed.
        assert_eq!(out, current);
    }

    #[test]
    fn migrate_is_idempotent() {
        let raw = json!({
            "name": "legacy",
            "nodes": [ { "id": "t", "kind": "trigger", "name": "Trigger" } ],
            "edges": []
        });

        let once = migrate(raw).expect("first migrate");
        let twice = migrate(once.clone()).expect("second migrate");
        assert_eq!(once, twice);
    }

    #[test]
    fn explicit_zero_schema_version_is_upgraded() {
        // An explicit `schema_version: 0` (predates the field semantically) is
        // stamped up to the current version.
        let raw = json!({
            "schema_version": 0,
            "name": "legacy",
            "nodes": [],
            "edges": []
        });

        let upgraded = migrate(raw).expect("migrate");
        assert_eq!(upgraded["schema_version"], json!(CURRENT_SCHEMA_VERSION));
    }

    #[test]
    fn future_schema_version_is_rejected_not_downgraded() {
        // A document from a newer crate must error rather than be silently
        // rewritten down to the current version.
        let raw = json!({
            "schema_version": CURRENT_SCHEMA_VERSION + 1,
            "name": "from_the_future",
            "nodes": [],
            "edges": []
        });

        let err = migrate(raw.clone()).expect_err("future schema_version must error");
        assert!(
            matches!(
                err,
                crate::error::EngineError::Validation(ValidationError::SchemaVersionTooNew {
                    found,
                    supported,
                }) if found == CURRENT_SCHEMA_VERSION + 1 && supported == CURRENT_SCHEMA_VERSION
            ),
            "expected SchemaVersionTooNew, got {err:?}"
        );
    }

    #[test]
    fn non_object_input_is_returned_unchanged() {
        // migrate reads `schema_version` defensively (absent → 0) and only
        // stamps into `Value::Object`, so non-object JSON is passed through
        // untouched rather than panicking.
        assert_eq!(migrate(json!(42)).expect("number"), json!(42));
        assert_eq!(migrate(json!("text")).expect("string"), json!("text"));
        assert_eq!(migrate(json!([1, 2, 3])).expect("array"), json!([1, 2, 3]));
        assert_eq!(migrate(json!(null)).expect("null"), json!(null));
    }

    #[test]
    fn full_graph_round_trips_through_migrate() {
        // A complete graph migrates, deserializes, re-serializes, and migrates
        // again to the same typed model.
        let raw = json!({
            "schema_version": 1,
            "id": "wf_1",
            "name": "demo",
            "nodes": [
                { "id": "t", "kind": "trigger", "type_version": 1, "name": "Trigger",
                  "config": { "trigger_kind": "manual" } },
                { "id": "a", "kind": "agent", "type_version": 1, "name": "Agent" }
            ],
            "edges": [
                { "from_node": "t", "from_port": "main", "to_node": "a", "to_port": "main" }
            ]
        });

        let upgraded = migrate(raw).expect("migrate");
        let graph: WorkflowGraph =
            serde_json::from_value(upgraded).expect("deserialize migrated graph");

        let reserialized = serde_json::to_value(&graph).expect("serialize");
        let remigrated = migrate(reserialized).expect("re-migrate");
        let graph_again: WorkflowGraph =
            serde_json::from_value(remigrated).expect("deserialize again");

        assert_eq!(graph, graph_again);
        assert_eq!(graph.nodes.len(), 2);
        assert_eq!(graph.edges.len(), 1);
    }

    // --- property-based tests ---------------------------------------------
    //
    // `migrate` is a pure JSON-to-JSON function applied to untrusted persisted
    // input, so it must never panic and must be idempotent. A bounded JSON
    // strategy exercises objects, arrays, and scalars — including malformed
    // `schema_version` shapes — cheaply.

    use proptest::prelude::*;

    /// A bounded, recursive `serde_json::Value` strategy: shallow nesting with a
    /// few elements per level so migration runs fast over arbitrary documents.
    fn arb_json() -> impl Strategy<Value = Value> {
        let leaf = prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::from),
            any::<i64>().prop_map(Value::from),
            "[A-Za-z0-9_]{0,8}".prop_map(Value::from),
        ];
        leaf.prop_recursive(3, 16, 4, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..4).prop_map(Value::from),
                prop::collection::hash_map("[A-Za-z_][A-Za-z0-9_]{0,5}", inner, 0..4)
                    .prop_map(|m| Value::from(m.into_iter().collect::<serde_json::Map<_, _>>())),
            ]
        })
    }

    /// A strategy that sometimes injects a `schema_version` field of an
    /// arbitrary JSON shape (integer, string, null, …) into an object, so the
    /// defensive `as_u64` read is exercised against non-integer versions.
    fn arb_json_with_schema_version() -> impl Strategy<Value = Value> {
        (arb_json(), prop::option::of(arb_json())).prop_map(|(base, maybe_version)| {
            let mut map = match base {
                Value::Object(m) => m,
                other => {
                    let mut m = serde_json::Map::new();
                    m.insert("inner".to_string(), other);
                    m
                }
            };
            if let Some(version) = maybe_version {
                map.insert("schema_version".to_string(), version);
            }
            Value::Object(map)
        })
    }

    proptest! {
        /// `migrate` never panics on arbitrary bounded JSON input.
        #[test]
        fn prop_migrate_never_panics(value in arb_json()) {
            let _ = migrate(value);
        }

        /// `migrate` never panics even when `schema_version` has a bogus shape.
        #[test]
        fn prop_migrate_never_panics_with_schema_version(value in arb_json_with_schema_version()) {
            let _ = migrate(value);
        }

        /// Migration is idempotent: re-migrating an already-migrated value is a
        /// no-op in value.
        #[test]
        fn prop_migrate_is_idempotent(value in arb_json_with_schema_version()) {
            if let Ok(once) = migrate(value) {
                let twice = migrate(once.clone()).expect("second migrate cannot fail");
                prop_assert_eq!(once, twice);
            }
        }

        /// Any `Ok` result that is a JSON object carries the current schema
        /// version stamp; non-object inputs pass through unstamped.
        #[test]
        fn prop_migrate_object_is_stamped(value in arb_json_with_schema_version()) {
            if let Ok(out) = migrate(value)
                && let Value::Object(map) = &out
            {
                prop_assert_eq!(
                    map.get("schema_version").and_then(Value::as_u64),
                    Some(u64::from(CURRENT_SCHEMA_VERSION))
                );
            }
        }
    }
}
