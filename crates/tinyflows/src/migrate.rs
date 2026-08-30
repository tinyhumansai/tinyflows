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
#[path = "migrate_tests.rs"]
mod tests;
