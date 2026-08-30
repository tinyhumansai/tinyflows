//! Tests for the namespaced key/value table.

use super::*;
use crate::flows::test_support::*;
use tempfile::TempDir;

#[test]
fn kv_get_set_round_trips_and_is_namespace_scoped() {
    let tmp = TempDir::new().unwrap();
    let dir = test_dir(&tmp);

    assert!(kv_get(&dir, "ns1", "k").unwrap().is_none());

    kv_set(&dir, "ns1", "k", &serde_json::json!({"v": 1})).unwrap();
    assert_eq!(
        kv_get(&dir, "ns1", "k").unwrap(),
        Some(serde_json::json!({"v": 1}))
    );

    // A different namespace does not see ns1's value.
    assert!(kv_get(&dir, "ns2", "k").unwrap().is_none());

    // Overwrite.
    kv_set(&dir, "ns1", "k", &serde_json::json!(2)).unwrap();
    assert_eq!(
        kv_get(&dir, "ns1", "k").unwrap(),
        Some(serde_json::json!(2))
    );
}

// ── require_approval ─────────────────────────────────────────────────────
