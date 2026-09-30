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
use crate::model::{CURRENT_SCHEMA_VERSION, WorkflowGraph};
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

/// Migrates raw graph JSON and deserializes it into a [`WorkflowGraph`],
/// attributing any failure to the member that caused it.
///
/// This is [`migrate`] followed by `serde_json::from_value`, **without** the
/// structural [`validate`](crate::validate) step, so a caller that wants every
/// structural error (via `validate::validate_all`) can run validation itself.
/// A failure here (an unmigratable schema, JSON that does not fit the model) is
/// genuinely a single error, whereas structural validation can surface many.
///
/// `serde_json` errors carry no path, and `missing field `name`` on its own is
/// unactionable: every field of `WorkflowGraph` is `#[serde(default)]`, so the
/// fault is always in a nested object, and a reader who takes it for the
/// top-level `name` they already set retries unchanged. On failure the error
/// therefore names the offending element (`nodes[1]: missing field `name``) or
/// top-level field (`name: invalid type: integer `123`, expected a string`).
///
/// # Errors
///
/// The [`migrate`] error, rendered, when migration refuses the document;
/// otherwise the located serde message described above. When the fault is not
/// in a single element or field (a non-array `nodes`, a non-object graph) the
/// bare serde message is returned rather than a guessed location.
///
/// # Examples
///
/// ```
/// use serde_json::json;
/// use tinyflows::migrate::deserialize_graph;
///
/// let err = deserialize_graph(json!({
///     "nodes": [
///         { "id": "start", "kind": "trigger", "name": "Trigger" },
///         { "id": "nameless", "kind": "trigger" }
///     ]
/// }))
/// .unwrap_err();
/// assert!(err.starts_with("nodes[1]: "), "{err}");
/// ```
pub fn deserialize_graph(value: Value) -> std::result::Result<WorkflowGraph, String> {
    let migrated = migrate(value).map_err(|e| e.to_string())?;
    serde_json::from_value::<WorkflowGraph>(migrated.clone())
        .map_err(|e| locate_graph_error(&migrated, &e))
}

/// The `WorkflowGraph` fields whose elements carry their own required fields.
const ELEMENT_ARRAYS: &[&str] = &["nodes", "inputs", "agents", "edges"];

/// Names the element a graph-level deserialization error came from.
///
/// Re-deserializes each member of the arrays that carry required fields and
/// reports the first that fails on its own, as `nodes[1]: <serde error>`. None
/// of these types use `deny_unknown_fields`, so an element that parses in
/// isolation is one the graph-level parse accepted too, and a failure found
/// here is the real fault rather than an artefact of checking it alone.
///
/// Runs only on the error path, and falls back to the bare message when the
/// fault is not in a single element -- a wrong type for `nodes` itself, say.
fn locate_graph_error(migrated: &Value, err: &serde_json::Error) -> String {
    // Re-parse with the element arrays emptied. If that still fails, the fault
    // is in the graph's own fields -- a non-string `name`, say -- and scanning
    // members would pin it on the first member that happens to be invalid too,
    // which is a confident wrong answer rather than a vague right one.
    let mut skeleton = migrated.clone();
    if let Some(fields) = skeleton.as_object_mut() {
        for field in ELEMENT_ARRAYS {
            if let Some(slot) = fields.get_mut(*field) {
                if slot.is_array() {
                    *slot = Value::Array(Vec::new());
                }
            }
        }
    }
    if serde_json::from_value::<WorkflowGraph>(skeleton).is_err() {
        return locate_top_level_error(migrated, err);
    }

    macro_rules! locate {
        ($field:literal, $ty:ty) => {
            if let Some(items) = migrated.get($field).and_then(Value::as_array) {
                for (index, item) in items.iter().enumerate() {
                    if let Err(inner) = serde_json::from_value::<$ty>(item.clone()) {
                        return format!("{}[{}]: {}", $field, index, inner);
                    }
                }
            }
        };
    }

    locate!("nodes", crate::model::Node);
    locate!("inputs", crate::model::WorkflowInput);
    locate!("agents", crate::model::AgentDefinition);
    locate!("edges", crate::model::Edge);

    err.to_string()
}

/// Names the graph's own field when the fault is at the top level.
///
/// `serde_json` reports a type mismatch as `invalid type: integer \`123\`,
/// expected a string` with **no field name** -- the same unactionable shape as
/// the missing-field case this helper exists to fix, so it gets the same
/// treatment.
///
/// Every `WorkflowGraph` field is `#[serde(default)]`, so an object carrying a
/// single field parses if and only if that field is valid. Probing one key at a
/// time therefore names the offender without a hardcoded field list. Unknown
/// keys parse (no `deny_unknown_fields`) and are skipped.
fn locate_top_level_error(migrated: &Value, err: &serde_json::Error) -> String {
    if let Some(fields) = migrated.as_object() {
        // A malformed collection can coexist with malformed members in a
        // different collection. Report the collection shape before probing
        // individual member collections, independent of JSON map key order.
        for key in ELEMENT_ARRAYS {
            if let Some(value) = fields.get(*key).filter(|value| !value.is_array()) {
                let probe =
                    Value::Object([((*key).to_string(), value.clone())].into_iter().collect());
                if let Err(inner) = serde_json::from_value::<WorkflowGraph>(probe) {
                    return format!("{key}: {inner}");
                }
            }
        }

        for (key, value) in fields {
            let probe = Value::Object([(key.clone(), value.clone())].into_iter().collect());
            if let Err(inner) = serde_json::from_value::<WorkflowGraph>(probe) {
                return format!("{key}: {inner}");
            }
        }
    }

    err.to_string()
}

#[cfg(test)]
#[path = "migrate_tests.rs"]
mod tests;
