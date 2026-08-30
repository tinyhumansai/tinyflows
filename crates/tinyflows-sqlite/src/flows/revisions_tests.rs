//! Tests for `update_flow_graph`'s guarded update, revision capture, and
//! auto-disarm behavior.

use super::*;
use crate::flows::definitions::{create_flow, set_enabled};
use crate::flows::test_support::*;
use rusqlite::Connection;
use tempfile::TempDir;

#[test]
fn update_flow_graph_bumps_updated_at_and_preserves_created_at() {
    let tmp = TempDir::new().unwrap();
    let dir = test_dir(&tmp);
    let flow = create_flow(&dir, "demo".to_string(), trigger_graph(), false, true).unwrap();

    let mut new_graph = trigger_graph();
    new_graph.name = "renamed-graph".to_string();
    let updated = update_flow_graph(
        &dir,
        &flow.id,
        "renamed".to_string(),
        new_graph,
        false,
        None,
        false,
        None,
    )
    .unwrap();

    assert_eq!(updated.name, "renamed");
    assert_eq!(updated.created_at, flow.created_at);
    assert_eq!(updated.graph.name, "renamed-graph");
}

/// The guarded UPDATE and the revision insert must commit as one unit. If the
/// revision insert fails (simulated here by dropping `flow_revisions` out
/// from under the store), the graph update must not have taken effect either
/// — a partial commit would mean a caller-observed error where the save
/// silently succeeded, with no revision to prove it.
#[test]
fn update_flow_graph_rolls_back_the_graph_update_when_revision_capture_fails() {
    let tmp = TempDir::new().unwrap();
    let dir = test_dir(&tmp);
    let flow = create_flow(&dir, "demo".to_string(), trigger_graph(), false, true).unwrap();

    // Sabotage the revision table so the INSERT inside the transaction fails.
    let db_path = dir.join("flows.db");
    let conn = Connection::open(&db_path).unwrap();
    conn.execute_batch("DROP TABLE flow_revisions;").unwrap();
    drop(conn);

    let mut new_graph = trigger_graph();
    new_graph.name = "renamed-graph".to_string();
    let err = update_flow_graph(
        &dir,
        &flow.id,
        "renamed".to_string(),
        new_graph,
        false,
        None,
        false,
        None,
    )
    .unwrap_err();
    assert!(matches!(err, FlowUpdateError::Store(_)));

    // The UPDATE must have rolled back with the failed revision insert: the
    // row must still read exactly as `create_flow` left it.
    let reloaded = get_flow(&dir, &flow.id)
        .unwrap()
        .expect("flow still present");
    assert_eq!(reloaded.name, "demo");
    assert_eq!(reloaded.updated_at, flow.updated_at);
    assert_eq!(reloaded.graph.name, flow.graph.name);
}

/// `enabled_override: None` must leave the persisted `enabled` column
/// exactly as it was — `update_flow_graph` re-reads the current row and
/// falls back to `current.enabled`, not to whatever the caller might have
/// observed earlier.
#[test]
fn update_flow_graph_with_none_override_preserves_current_enabled_column() {
    let tmp = TempDir::new().unwrap();
    let dir = test_dir(&tmp);
    let flow = create_flow(&dir, "demo".to_string(), trigger_graph(), false, true).unwrap();
    assert!(flow.enabled, "flow created enabled");

    let updated = update_flow_graph(
        &dir,
        &flow.id,
        flow.name.clone(),
        trigger_graph(),
        false,
        None,  // enabled_override
        false, // force_disarm_if_automatic
        None,
    )
    .unwrap();

    assert!(
        updated.enabled,
        "a None override must preserve the row's current enabled state"
    );
    let reloaded = get_flow(&dir, &flow.id).unwrap().unwrap();
    assert!(reloaded.enabled);
}

/// `enabled_override: Some(false)` must force-persist `enabled=false`
/// regardless of what the row's `enabled` column currently holds — this is
/// the mechanism `flows_update`'s B29 Rule 1 analogue relies on to disarm a
/// manual→automatic trigger transition in the same guarded write.
#[test]
fn update_flow_graph_with_some_false_override_forces_disabled() {
    let tmp = TempDir::new().unwrap();
    let dir = test_dir(&tmp);
    let flow = create_flow(&dir, "demo".to_string(), trigger_graph(), false, true).unwrap();
    assert!(flow.enabled, "flow created enabled");

    let updated = update_flow_graph(
        &dir,
        &flow.id,
        flow.name.clone(),
        trigger_graph(),
        false,
        Some(false), // enabled_override
        false,       // force_disarm_if_automatic
        None,
    )
    .unwrap();

    assert!(
        !updated.enabled,
        "a Some(false) override must force enabled=false even though the row was enabled"
    );
    let reloaded = get_flow(&dir, &flow.id).unwrap().unwrap();
    assert!(!reloaded.enabled);
}

/// Regression for the silent live-arming race Codex flagged on this PR:
/// `flows_update` (ops.rs) makes its manual→automatic disarm decision from
/// an *outer* `existing` read taken before `update_flow_graph`'s own guarded
/// UPDATE re-reads the row. If a concurrent `flows_set_enabled(id, true)`
/// landed in that gap — which bumps `updated_at`, so it would NOT trip the
/// optimistic-concurrency conflict — the outer read would be stale while the
/// row is actually enabled by write time. This proves the mechanism the fix
/// relies on to close that race: an `enabled_override` of `Some(false)`
/// (what `flows_update` now passes unconditionally on a manual→automatic
/// transition, never gated on the stale outer read) always wins over
/// whatever the row's `enabled` column was concurrently flipped to,
/// simulated here by flipping it with `set_enabled` between the two calls.
#[test]
fn update_flow_graph_override_wins_over_concurrently_enabled_row() {
    let tmp = TempDir::new().unwrap();
    let dir = test_dir(&tmp);
    let flow = create_flow(&dir, "demo".to_string(), trigger_graph(), false, false).unwrap();
    assert!(!flow.enabled, "flow created disabled");

    // Simulates a concurrent `flows_set_enabled(id, true)` racing in after
    // `flows_update`'s outer `existing` read observed `enabled: false`, but
    // before its guarded `update_flow_graph` write below.
    let raced = set_enabled(&dir, &flow.id, true).unwrap();
    assert!(raced.enabled);

    let updated = update_flow_graph(
        &dir,
        &flow.id,
        flow.name.clone(),
        trigger_graph(),
        false,
        Some(false), // the unconditional disarm override
        false,       // force_disarm_if_automatic
        None,
    )
    .unwrap();

    assert!(
        !updated.enabled,
        "the disarm override must win over a concurrently-enabled row, not the reverse"
    );
    let reloaded = get_flow(&dir, &flow.id).unwrap().unwrap();
    assert!(!reloaded.enabled);
}

/// R-m2 regression: the manual→automatic disarm decision must be computed
/// against the row `update_flow_graph` JUST re-read (`current`), never a
/// caller-supplied belief about the flow's prior state. Before the fix,
/// `ops::flows_update` computed this transition from an OUTER `existing`
/// read taken before calling into the store — a concurrent write between
/// that read and this call could make the transition invisible to the
/// caller, letting an automatic-trigger graph persist `enabled: true`.
///
/// Proven here without needing to fake a race: the disarm must fire from
/// `current.graph` (MANUAL) vs the new `graph` (automatic) alone, and must
/// WIN over an `enabled_override` that explicitly asks to stay enabled —
/// exactly the shape of override a stale caller-side decision could
/// otherwise have smuggled through.
#[test]
fn update_flow_graph_disarms_transition_from_the_fresh_row_even_when_override_asks_to_stay_enabled()
{
    let tmp = TempDir::new().unwrap();
    let dir = test_dir(&tmp);
    let flow = create_flow(&dir, "demo".to_string(), trigger_graph(), false, true).unwrap();
    assert!(flow.enabled, "flow created enabled");

    let updated = update_flow_graph(
        &dir,
        &flow.id,
        flow.name.clone(),
        automatic_schedule_graph(),
        false,
        Some(true), // caller explicitly asks to stay enabled
        false,      // force_disarm_if_automatic (the remote-authoring flag) OFF —
        // proving the unconditional Rule 1 transition-disarm fires on its own
        None,
    )
    .unwrap();

    assert!(
        !updated.enabled,
        "a manual->automatic transition must disarm even when enabled_override asks to stay \
         enabled — the disarm always wins (R-m2)"
    );
    let reloaded = get_flow(&dir, &flow.id).unwrap().unwrap();
    assert!(!reloaded.enabled);
}

/// Sibling of the above: when there is NO transition (the row was already
/// automatic before this call, matching what's actually in the DB right
/// now), an ordinary `enabled_override` is honoured normally — the fix must
/// not over-disarm every automatic-trigger update, only genuine
/// manual/none → automatic transitions (unless `force_disarm_if_automatic`
/// is also set).
#[test]
fn update_flow_graph_does_not_disarm_an_automatic_to_automatic_update() {
    let tmp = TempDir::new().unwrap();
    let dir = test_dir(&tmp);
    let flow = create_flow(
        &dir,
        "demo".to_string(),
        automatic_schedule_graph(),
        false,
        false,
    )
    .unwrap();
    assert!(!flow.enabled, "born disabled — armed explicitly next");
    let armed = set_enabled(&dir, &flow.id, true).unwrap();
    assert!(armed.enabled);

    let updated = update_flow_graph(
        &dir,
        &flow.id,
        flow.name.clone(),
        automatic_schedule_graph(),
        false,
        None,  // no explicit override — preserve current.enabled
        false, // force_disarm_if_automatic OFF
        None,
    )
    .unwrap();

    assert!(
        updated.enabled,
        "an automatic->automatic update (no transition) must not be auto-disarmed"
    );
}

#[test]
fn update_flow_graph_can_change_require_approval() {
    let tmp = TempDir::new().unwrap();
    let dir = test_dir(&tmp);
    let flow = create_flow(&dir, "demo".to_string(), trigger_graph(), false, true).unwrap();
    assert!(!flow.require_approval);

    let updated = update_flow_graph(
        &dir,
        &flow.id,
        flow.name.clone(),
        trigger_graph(),
        true,
        None,
        false,
        None,
    )
    .unwrap();
    assert!(updated.require_approval);

    let reloaded = get_flow(&dir, &flow.id).unwrap().unwrap();
    assert!(reloaded.require_approval);
}
