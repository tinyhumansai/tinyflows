//! A stable content hash over a graph and its approval flag.
//!
//! A run that parks for approval records this hash; when it is resumed the host
//! recomputes it and refuses to continue if the graph (or the approval setting)
//! changed while the run was parked. Because the value is persisted, the digest
//! is a compatibility surface: it must stay byte-for-byte identical across
//! releases, so the tests pin a fixed vector.

use serde_json::Value;
use sha2::{Digest, Sha256};
use tinyflows::model::WorkflowGraph;

/// Hashes `graph` together with `require_approval` into a lowercase hex SHA-256.
///
/// The hash is over graph *content*, never incidental key order: the graph is
/// serialized to a JSON value and every object's keys are recursively sorted
/// before the value is rendered, so two graphs that differ only in the order
/// `serde_json` happened to emit their object keys hash identically. Array
/// order is preserved because it is semantically meaningful.
///
/// Returns `None` (never panics) when the graph fails to serialize. The two
/// sides of a resume treat `None` differently: at park time it simply stores no
/// pin, so the run later takes the legacy "unknown, allow with a warning" path;
/// at resume time `Some(expected) != None` is a mismatch, so the run is refused.
/// A hash failure therefore fails closed on resume.
#[must_use]
pub fn compute_graph_hash(graph: &WorkflowGraph, require_approval: bool) -> Option<String> {
    let raw = match serde_json::to_value(graph) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                target: "flows",
                error = %e,
                "[flows] compute_graph_hash: failed to serialize graph to JSON — proceeding without a graph pin"
            );
            return None;
        }
    };
    let raw = serde_json::json!({ "graph": raw, "require_approval": require_approval });
    let canonical = canonicalize_json(&raw);
    let serialized = match serde_json::to_string(&canonical) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                target: "flows",
                error = %e,
                "[flows] compute_graph_hash: failed to serialize canonicalized graph — proceeding without a graph pin"
            );
            return None;
        }
    };
    let digest = Sha256::digest(serialized.as_bytes());
    Some(hex::encode(digest))
}

/// Recursively rewrites every JSON object's keys into sorted order, leaving
/// arrays (whose element order is semantically meaningful) and scalars
/// unchanged.
///
/// Sorting is explicit rather than trusting `serde_json`'s default map order,
/// which is sorted only when the `preserve_order` feature is off; another crate
/// in the build enabling it would otherwise change every persisted hash.
fn canonicalize_json(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut sorted = serde_json::Map::new();
            for key in keys {
                sorted.insert(key.clone(), canonicalize_json(&map[key]));
            }
            Value::Object(sorted)
        }
        Value::Array(items) => Value::Array(items.iter().map(canonicalize_json).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
#[path = "graph_hash_tests.rs"]
mod tests;
