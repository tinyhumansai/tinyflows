//! Incremental per-node step persistence for a live flow run.

use anyhow::{Context, Result};
use rusqlite::params;
use std::path::Path;
use tinyflows_catalog::FlowRunStep;

use super::{with_connection, with_immediate_transaction};

/// Incrementally upserts a single [`FlowRunStep`] onto a live `flow_runs`
/// row's `steps_json`, keyed by `node_id` — used by the run observer
/// (`flows::observability::FlowRunObserver`) to persist each node's step **as
/// it finishes** (issue G2, live run observation) rather than only rebuilding
/// the whole step list at settle.
///
/// **`BEGIN IMMEDIATE`-guarded read-modify-write (R-m1).** Each call opens its
/// own connection (see `with_connection`), so without an explicit transaction
/// two observer callbacks firing for parallel branch nodes of the *same* run
/// can interleave: both read `steps_json = [A]`, one writes `[A,B]`, the other
/// writes `[A,C]` — B is silently lost from the live view, and lost for good,
/// since the post-hoc `settle_steps` reconstruction only refills a missing
/// node with `status: None` rather than recovering the real outcome/duration.
/// `BEGIN IMMEDIATE` takes SQLite's write lock up front (rather than only at
/// the final `UPDATE`, which is what a plain autocommit read-then-write would
/// do), so a concurrent upsert either waits (covered by this store's
/// `busy_timeout = 5000` connection pragma — see `with_connection`) or is
/// serialized behind it; there is no window in which both readers can observe
/// the same pre-write `steps_json`. Kept deliberately minimal (one SELECT, one
/// UPDATE) to bound how long the write lock is held.
///
/// A re-run of the same `node_id` (a retry, or a resumed run re-touching a
/// node) replaces its prior entry rather than duplicating it, so the
/// persisted list stays one entry per node. No-op if the run's start row
/// hasn't been inserted yet (nothing to update) — mirrors the best-effort
/// contract of the run-row writers in `flows::ops`.
pub fn upsert_flow_run_step(dir: &Path, run_id: &str, step: &FlowRunStep) -> Result<()> {
    use rusqlite::OptionalExtension;
    with_connection(dir, |conn| {
        with_immediate_transaction(conn, |conn| {
            let existing: Option<String> = conn
                .query_row(
                    "SELECT steps_json FROM flow_runs WHERE id = ?1",
                    params![run_id],
                    |row| row.get(0),
                )
                .optional()
                .context("Failed to read flow run steps for incremental upsert")?;
            let Some(raw) = existing else {
                tracing::debug!(target: "flows", run_id, node = %step.node_id, "[flows] upsert_flow_run_step: no run row yet — skipping incremental step persist");
                return Ok(());
            };
            let mut steps: Vec<FlowRunStep> = serde_json::from_str(&raw)
                .context("Failed to deserialize existing flow run steps")?;
            match steps.iter_mut().find(|s| s.node_id == step.node_id) {
                Some(slot) => *slot = step.clone(),
                None => steps.push(step.clone()),
            }
            let steps_json =
                serde_json::to_string(&steps).context("Failed to serialize flow run steps")?;
            conn.execute(
                "UPDATE flow_runs SET steps_json = ?1 WHERE id = ?2",
                params![steps_json, run_id],
            )
            .context("Failed to persist incremental flow run step")?;
            tracing::debug!(target: "flows", run_id, node = %step.node_id, step_count = steps.len(), "[flows] persisted incremental flow run step");
            Ok(())
        })
    })
}

#[cfg(test)]
#[path = "run_steps_tests.rs"]
mod tests;
