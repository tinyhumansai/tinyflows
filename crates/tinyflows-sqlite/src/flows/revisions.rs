//! Flow revision history: `update_flow_graph`'s guarded-update-plus-
//! revision-capture transaction, and reading revisions back.

use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::params;
use std::path::Path;
use tinyflows_catalog::{Flow, FlowRevision};
use uuid::Uuid;

use super::definitions::get_flow;
use super::{sql_conversion_error, with_connection, with_immediate_transaction};

/// How many revision snapshots to retain per flow (audit F6). Older ones are
/// pruned on each new capture.
const MAX_REVISIONS_PER_FLOW: usize = 20;

/// Failure modes of [`update_flow_graph`] that the caller must distinguish:
/// a genuine not-found, an optimistic-concurrency conflict (carrying the
/// current server flow so the UI can diff/reload), or a store error.
#[derive(Debug)]
pub enum FlowUpdateError {
    /// No flow with that id exists.
    NotFound,
    /// The flow changed since `expected_updated_at` was observed — the write
    /// was refused to avoid clobbering. Carries the current server flow.
    Conflict(Box<Flow>),
    /// An underlying store failure.
    Store(anyhow::Error),
}

impl std::fmt::Display for FlowUpdateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "flow not found"),
            Self::Conflict(_) => write!(f, "flow changed since it was loaded"),
            Self::Store(e) => write!(f, "{e}"),
        }
    }
}

/// Replaces a flow's name/graph/`require_approval` (re-validated by the caller
/// before this is invoked) in place, bumping `updated_at`, capturing the prior
/// graph as a revision, and enforcing optimistic concurrency.
///
/// When `expected_updated_at` is `Some`, the write is refused with
/// [`FlowUpdateError::Conflict`] (carrying the current server flow) if the
/// flow's `updated_at` no longer matches — so an agent save and a concurrent
/// canvas save can't silently clobber each other. `None` keeps the prior
/// last-write-wins behaviour for callers that don't track a version.
///
/// `enabled_override`, when `Some`, forces the persisted `enabled` flag to
/// that value in the *same* guarded `UPDATE` as the graph/name/
/// `require_approval` write. `None` leaves `enabled` untouched (falls back to
/// the freshly re-read `current.enabled`), matching the previous behaviour
/// for every other caller.
///
/// `force_disarm_if_automatic`, when `true`, unconditionally disarms
/// (`enabled: false`) if the resulting graph (`graph`) has an automatic
/// trigger — used by `ops::flows_update_disarming_automatic` for remote
/// authoring surfaces.
///
/// **R-m2:** independent of `force_disarm_if_automatic`, this ALWAYS disarms
/// on a manual/none → automatic trigger transition (the B29 Rule 1 analogue)
/// — computed here, against the row this call just re-read
/// (`current.graph`), rather than trusting a transition flag the caller
/// derived from an earlier, possibly-stale read. `update_flow_graph`'s own
/// guarded `UPDATE` below keys its `WHERE` clause on this exact `current`
/// row, so this is the only read of "was it automatic before" that can't
/// have gone stale between computing the decision and writing it. An
/// `enabled_override` supplied by the caller can never re-arm a graph this
/// check disarms — the disarm always wins.
// One argument per column this write sets, which is what a row update is. A
// params struct here would be the row spelled a second time, and every call
// site would gain a constructor that names the same fields in the same order.
#[allow(clippy::too_many_arguments)]
pub fn update_flow_graph(
    dir: &Path,
    id: &str,
    name: String,
    graph: tinyflows::model::WorkflowGraph,
    require_approval: bool,
    enabled_override: Option<bool>,
    force_disarm_if_automatic: bool,
    expected_updated_at: Option<&str>,
) -> std::result::Result<Flow, FlowUpdateError> {
    let current = get_flow(dir, id)
        .map_err(FlowUpdateError::Store)?
        .ok_or(FlowUpdateError::NotFound)?;

    // Optimistic-concurrency check: refuse if the flow moved on since the
    // caller observed `expected_updated_at`.
    if let Some(expected) = expected_updated_at {
        if current.updated_at != expected {
            return Err(FlowUpdateError::Conflict(Box::new(current)));
        }
    }

    // R-m2: `was_auto` MUST come from `current` (just re-read above, right
    // before the guarded UPDATE below), never from a caller-observed
    // snapshot — a concurrent write between an ops-level read and this call
    // would otherwise let a manual→automatic transition slip past
    // undetected and persist `enabled: true` on an automatic-trigger graph.
    let now_auto = tinyflows_catalog::graph_policy::trigger_is_automatic(&graph);
    let was_auto = tinyflows_catalog::graph_policy::trigger_is_automatic(&current.graph);
    let is_manual_to_auto_transition = now_auto && !was_auto;
    let forced_automatic_disarm = force_disarm_if_automatic && now_auto;
    let auto_disarm = is_manual_to_auto_transition || forced_automatic_disarm;
    if auto_disarm {
        tracing::debug!(
            target: "flows",
            flow_id = %id,
            was_auto,
            now_auto,
            is_manual_to_auto_transition,
            forced_automatic_disarm,
            "[flows] update_flow_graph: disarming — automatic-trigger transition detected \
             against the freshly re-read row (R-m2)"
        );
    }

    let graph_json = serde_json::to_string(&graph)
        .context("Failed to serialize graph")
        .map_err(FlowUpdateError::Store)?;
    // Never fall back to a placeholder here: a revision row exists to prove
    // what the graph looked like before this save, so a serialization
    // failure must fail the whole save rather than silently write a `null`
    // graph into the audit trail.
    let prior_graph_json = serde_json::to_string(&current.graph)
        .context("Failed to serialize prior graph for revision capture")
        .map_err(FlowUpdateError::Store)?;
    let now = Utc::now().to_rfc3339();
    let new_enabled = if auto_disarm {
        false
    } else {
        enabled_override.unwrap_or(current.enabled)
    };

    with_connection(dir, |conn| {
        // The guarded UPDATE, the revision insert, and the prune must commit
        // as one unit: without a transaction, each `execute` autocommits on
        // its own, so a failure between the UPDATE and the INSERT (disk full,
        // a damaged revision table, …) leaves the graph changed with no
        // revision recorded — silently violating the "every save is
        // reversible" contract the revision table exists for.
        with_immediate_transaction(conn, |conn| {
            // Guarded UPDATE keyed on the observed updated_at (race-safe even
            // without an explicit expected version) — a concurrent writer that
            // moved updated_at makes this match 0 rows. Targeted columns only, so a
            // concurrent set_enabled/record_run isn't clobbered (unless this call
            // itself carries an `enabled_override`, in which case `enabled` is
            // one of the targeted columns by design).
            let changed = conn
                .execute(
                    "UPDATE flow_definitions SET name = ?1, graph_json = ?2, updated_at = ?3, \
                 require_approval = ?4, enabled = ?5 WHERE id = ?6 AND updated_at = ?7",
                    params![
                        name,
                        graph_json,
                        now,
                        if require_approval { 1 } else { 0 },
                        if new_enabled { 1 } else { 0 },
                        id,
                        current.updated_at,
                    ],
                )
                .context("Failed to update flow")?;
            if changed == 0 {
                // Someone raced us between the read and the write.
                anyhow::bail!("__conflict__");
            }
            // Capture the prior graph as a revision, then prune to the cap.
            let rev_id = Uuid::new_v4().to_string();
            conn.execute(
                "INSERT INTO flow_revisions (id, flow_id, graph_json, name, require_approval, \
             created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    rev_id,
                    id,
                    prior_graph_json,
                    current.name,
                    if current.require_approval { 1 } else { 0 },
                    now,
                ],
            )
            .context("Failed to record flow revision")?;
            conn.execute(
                "DELETE FROM flow_revisions WHERE flow_id = ?1 AND id NOT IN (\
                SELECT id FROM flow_revisions WHERE flow_id = ?1 \
                ORDER BY created_at DESC, id DESC LIMIT ?2)",
                params![id, MAX_REVISIONS_PER_FLOW as i64],
            )
            .context("Failed to prune flow revisions")?;
            Ok(())
        })
    })
    .map_err(|e| {
        if e.to_string().contains("__conflict__") {
            // Re-read to hand back the current state.
            match get_flow(dir, id) {
                Ok(Some(f)) => FlowUpdateError::Conflict(Box::new(f)),
                Ok(None) => FlowUpdateError::NotFound,
                Err(e) => FlowUpdateError::Store(e),
            }
        } else {
            FlowUpdateError::Store(e)
        }
    })?;

    get_flow(dir, id)
        .map_err(FlowUpdateError::Store)?
        .ok_or(FlowUpdateError::NotFound)
}

/// Lists a flow's revision snapshots, newest first, up to `limit`.
pub fn list_revisions(dir: &Path, flow_id: &str, limit: usize) -> Result<Vec<FlowRevision>> {
    with_connection(dir, |conn| {
        let mut stmt = conn.prepare(
            "SELECT id, flow_id, graph_json, name, require_approval, created_at \
             FROM flow_revisions WHERE flow_id = ?1 ORDER BY created_at DESC, id DESC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![flow_id, limit as i64], map_revision_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })
}

/// Fetches one revision by id (scoped to `flow_id`), or `None`.
pub fn revision_by_id(
    dir: &Path,
    flow_id: &str,
    revision_id: &str,
) -> Result<Option<FlowRevision>> {
    with_connection(dir, |conn| {
        let mut stmt = conn.prepare(
            "SELECT id, flow_id, graph_json, name, require_approval, created_at \
             FROM flow_revisions WHERE flow_id = ?1 AND id = ?2",
        )?;
        let mut rows = stmt.query_map(params![flow_id, revision_id], map_revision_row)?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    })
}

fn map_revision_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<FlowRevision> {
    let graph_str: String = row.get(2)?;
    // A stored revision's `graph_json` was written by a successful
    // `serde_json::to_string` at capture time (see `update_flow_graph`), so a
    // decode failure here means the row is corrupt — surface it rather than
    // quietly returning a `null` graph that reads as "empty" instead of
    // "broken".
    let graph: serde_json::Value =
        serde_json::from_str(&graph_str).map_err(sql_conversion_error)?;
    Ok(FlowRevision {
        id: row.get(0)?,
        flow_id: row.get(1)?,
        graph,
        name: row.get(3)?,
        require_approval: row.get::<_, i64>(4)? != 0,
        created_at: row.get(5)?,
    })
}

#[cfg(test)]
#[path = "revisions_tests.rs"]
mod tests;
