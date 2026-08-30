//! `flow_runs` CRUD: insert, prune, finish, list, and the parked-run
//! expiry / resume-tracking helpers.

use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use std::path::Path;
use tinyflows_catalog::{FlowRun, FlowRunStep};

use super::{sql_conversion_error, with_connection};

/// Shared column list for every `flow_runs` SELECT — keeps
/// [`map_flow_run_row`]'s positional `row.get(N)` calls in sync.
const FLOW_RUN_COLUMNS: &str = "id, flow_id, thread_id, status, started_at, finished_at, \
     steps_json, pending_approvals_json, error, graph_hash";

/// Default per-flow run-history retention cap: how many of the most-recent runs
/// a single flow keeps before older *terminal* runs are pruned on the next
/// insert (and by the manual `flows_prune_runs` sweep). Bounds unbounded
/// `flow_runs` growth for a hot, frequently-triggered flow while keeping enough
/// history for the run-history inspector.
///
/// Non-terminal runs (`running`, `pending_approval`) are **never** pruned — a
/// parked `pending_approval` run must survive so a later `flows_resume` can find
/// it — so the effective row count for a flow may briefly exceed this cap by the
/// number of live/parked runs. See [`prune_flow_runs`].
pub const MAX_FLOW_RUNS_PER_FLOW: usize = 100;

/// Inserts the initial `"running"` row for a new `flows_run` / `flows_resume`
/// invocation. `id` and `thread_id` are the same value in practice (the
/// tinyflows checkpointer thread id doubles as the run's stable identifier),
/// kept as two columns because they answer two different questions (row
/// identity vs. the checkpointer key `flows_resume` needs).
pub fn insert_flow_run(
    dir: &Path,
    id: &str,
    flow_id: &str,
    thread_id: &str,
    started_at: &str,
) -> Result<()> {
    with_connection(dir, |conn| {
        conn.execute(
            "INSERT INTO flow_runs (id, flow_id, thread_id, status, started_at)
             VALUES (?1, ?2, ?3, 'running', ?4)",
            params![id, flow_id, thread_id, started_at],
        )
        .context("Failed to insert flow run")?;
        // Retention: prune older terminal runs for this flow on every new-run
        // insert, so `flow_runs` stays bounded for a hot flow. Same connection
        // as the insert — atomic w.r.t. this write. A pruning failure is not
        // fatal to the insert (the run itself matters more than trimming
        // history), so it's logged and swallowed.
        if let Err(e) = prune_flow_runs_conn(conn, flow_id, MAX_FLOW_RUNS_PER_FLOW) {
            tracing::warn!(target: "flows", flow_id, error = %e, "[flows] insert_flow_run: retention prune failed (insert kept)");
        }
        Ok(())
    })
}

/// Prunes a flow's run history down to at most `keep` of its most-recent runs,
/// deleting any row outside the newest-`keep` window whose `status` is NOT
/// `running` or `pending_approval` — that is every terminal status this store
/// can hold (`completed`, `completed_with_warnings`, `failed`, `cancelled`,
/// `interrupted`, and any future status this host doesn't recognize yet), not
/// just the `completed`/`failed`/`cancelled` trio. The two excluded statuses
/// are the only ones that are never deleted — a parked `pending_approval` run
/// must never be pruned out from under a pending `flows_resume`, and a
/// `running` row belongs to a live task. Returns the number of rows deleted.
///
/// `keep` is clamped to at least 1. Exposed for the manual `flows_prune_runs`
/// sweep; the new-run insert path calls the connection-scoped helper directly.
pub fn prune_flow_runs(dir: &Path, flow_id: &str, keep: usize) -> Result<usize> {
    with_connection(dir, |conn| prune_flow_runs_conn(conn, flow_id, keep))
}

/// Connection-scoped core of [`prune_flow_runs`] — see its doc. Kept separate so
/// the new-run insert path can prune inside its own `with_connection` block
/// without reopening the database.
fn prune_flow_runs_conn(conn: &Connection, flow_id: &str, keep: usize) -> Result<usize> {
    let keep = i64::try_from(keep.max(1)).context("Run retention cap overflow")?;
    let deleted = conn
        .execute(
            "DELETE FROM flow_runs
              WHERE flow_id = ?1
                AND status NOT IN ('running', 'pending_approval')
                AND id NOT IN (
                    SELECT id FROM flow_runs
                     WHERE flow_id = ?1
                     ORDER BY started_at DESC, id DESC
                     LIMIT ?2
                )",
            params![flow_id, keep],
        )
        .context("Failed to prune flow runs")?;
    if deleted > 0 {
        tracing::debug!(target: "flows", flow_id, deleted, keep, "[flows] pruned old terminal flow runs past retention cap");
    }
    Ok(deleted)
}

/// Finalizes a flow run row: settles its terminal `status`, `finished_at`,
/// reconstructed `steps`, `pending_approvals`, and (on failure) `error`.
/// Called once a `flows_run` / `flows_resume` invocation settles — including
/// the timeout / capability-error paths, so a row never gets stuck at
/// `"running"` when the process is still up.
///
/// **Guarded write (R-M2).** The `UPDATE` only matches a row that is still
/// live — `status IN ('running','pending_approval')` — mirroring the same
/// re-check [`expire_parked_runs`] and [`mark_run_interrupted`] already do.
/// Without it this was an unconditional `WHERE id = ?`, so a caller that read a
/// non-terminal status and then lost a race could overwrite a row that had
/// meanwhile settled: `flows_cancel_run` reads `running`, the live run finishes
/// `completed` and deregisters, `run_registry::cancel` returns `false`, and the
/// "not in flight" branch then relabels a fully-completed run (whose real side
/// effects fired) as `cancelled`. Returns whether a row was actually updated so
/// callers can log the no-op instead of silently believing the write landed.
///
/// `graph_hash` (T-M1) is `Some(hash)` only when this write is the one that
/// *parks* the row (`status == "pending_approval"`) — it pins the content hash
/// of the graph the checkpoint was taken against, so a later `flows_resume`
/// can refuse if `save_workflow` rewrote the flow in the meantime. Every other
/// write passes `None`, which clears any stale pin once the row leaves
/// `pending_approval` (a settled row has no further use for it).
// One argument per column this write sets, which is what a row update is. A
// params struct here would be the row spelled a second time, and every call
// site would gain a constructor that names the same fields in the same order.
#[allow(clippy::too_many_arguments)]
pub fn finish_flow_run(
    dir: &Path,
    id: &str,
    status: &str,
    finished_at: &str,
    steps: &[FlowRunStep],
    pending_approvals: &[String],
    error: Option<&str>,
    graph_hash: Option<&str>,
) -> Result<bool> {
    let steps_json = serde_json::to_string(steps).context("Failed to serialize flow run steps")?;
    let pending_json = serde_json::to_string(pending_approvals)
        .context("Failed to serialize flow run pending approvals")?;
    with_connection(dir, |conn| {
        let updated = conn
            .execute(
                "UPDATE flow_runs SET status = ?1, finished_at = ?2, steps_json = ?3, \
                 pending_approvals_json = ?4, error = ?5, graph_hash = ?6 \
                 WHERE id = ?7 AND status IN ('running', 'pending_approval')",
                params![
                    status,
                    finished_at,
                    steps_json,
                    pending_json,
                    error,
                    graph_hash,
                    id
                ],
            )
            .context("Failed to finish flow run")?;
        Ok(updated > 0)
    })
}

/// Expires every parked `pending_approval` run whose "parked since" timestamp
/// (`COALESCE(finished_at, started_at)` — a run's `finished_at` is stamped when
/// it pauses at a gate) is strictly older than `cutoff` (an RFC3339 instant),
/// transitioning it to a terminal `"cancelled"` status stamped `now` with
/// `error_msg`. Returns the `(run_id, flow_id)` of the runs **actually flipped**
/// so the caller can update the flow summary, publish `FlowRunFinished`, and
/// drop the durable checkpoint (issue G4 — parked-run TTL) for real settles
/// only.
///
/// **Candidates are not sweeps.** The `SELECT` and each row's guarded `UPDATE`
/// are separate statements on an autocommit connection (`with_connection` opens
/// a fresh connection per call, not a transaction spanning this function), so a
/// concurrent `mark_run_resuming` on another connection can land in between: the
/// row was `pending_approval` at `SELECT` time and no longer is when its own
/// `UPDATE` runs. The per-row `WHERE status = 'pending_approval'` re-check keeps
/// that row's data safe — but returning the unfiltered candidate list would let
/// the caller act on a run it never actually expired: dropping the checkpoint out
/// from under a resume that just claimed it, and publishing a terminal
/// `FlowRunFinished` for a run still executing. That false event is the worse
/// half, because the frontend de-dupes terminal events by `${flow_id}:${run_id}`
/// — so the run's real completion would later be discarded as an alias replay,
/// leaving a successful run displayed as cancelled. Only rows whose `UPDATE`
/// reports `changed > 0` are returned.
///
/// RFC3339 timestamps produced by `chrono::Utc::…to_rfc3339()` all carry the
/// same `+00:00` offset, so a lexicographic `<` is a valid chronological
/// comparison here. Best-effort by contract at the call site: the update runs
/// under the same WAL + `busy_timeout` connection as every other write.
pub fn expire_parked_runs(
    dir: &Path,
    cutoff: &str,
    now: &str,
    error_msg: &str,
) -> Result<Vec<(String, String)>> {
    with_connection(dir, |conn| {
        let mut stmt = conn.prepare(
            "SELECT id, flow_id FROM flow_runs
             WHERE status = 'pending_approval'
               AND COALESCE(finished_at, started_at) < ?1",
        )?;
        let stale: Vec<(String, String)> = stmt
            .query_map(params![cutoff], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        drop(stmt);

        let mut swept = Vec::with_capacity(stale.len());
        for (run_id, flow_id) in stale {
            // Re-check the status in the WHERE so a run resumed/cancelled
            // between the SELECT and here is not clobbered, and keep only the
            // rows this sweep genuinely flipped — see the fn doc.
            let changed = conn
                .execute(
                    "UPDATE flow_runs SET status = 'cancelled', finished_at = ?1, error = ?2 \
                     WHERE id = ?3 AND status = 'pending_approval'",
                    params![now, error_msg, &run_id],
                )
                .context("Failed to expire parked flow run")?;
            if changed > 0 {
                swept.push((run_id, flow_id));
            } else {
                tracing::debug!(
                    target: "flows",
                    run_id = %run_id,
                    "[flows] TTL sweep: run left 'pending_approval' concurrently — not expiring it"
                );
            }
        }
        if !swept.is_empty() {
            tracing::info!(target: "flows", swept = swept.len(), "[flows] expired parked pending_approval runs past TTL");
        }
        Ok(swept)
    })
}

/// Lists the `(id, flow_id)` of every run persisted at `status = 'running'`
/// whose `started_at` is strictly **before** `started_before` (RFC3339). Used by
/// the boot-time orphan sweep (bug B42): after a crash/restart no in-process
/// task is executing these rows, so
/// `ops::sweep_orphaned_running_runs_on_boot`
/// reconciles each one that isn't backed by a live in-flight run to a terminal
/// `'interrupted'` via [`mark_run_interrupted`].
///
/// The `started_before` floor is what makes the sweep provably unable to touch
/// a run **this** process started: the sweep passes the instant this process
/// first entered the flow-run lifecycle, and every row this process inserts is
/// stamped at or after that instant. Without it, the sweep's only guard is the
/// in-flight registry, which a row briefly escapes between `start_flow_run_row`
/// and `run_registry::register`. `started_at` is a fixed-shape UTC RFC3339
/// string, so the lexicographic `<` matches chronological order (same
/// comparison the parked-run TTL sweep already relies on).
pub fn list_running_run_ids(dir: &Path, started_before: &str) -> Result<Vec<(String, String)>> {
    with_connection(dir, |conn| {
        let mut stmt = conn.prepare(
            "SELECT id, flow_id FROM flow_runs WHERE status = 'running' AND started_at < ?1",
        )?;
        let rows: Vec<(String, String)> = stmt
            .query_map(params![started_before], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    })
}

/// Test-only unconditional status write, bypassing the
/// [`finish_flow_run`] liveness guard.
///
/// Production code must never do a terminal → terminal transition — that is the
/// corruption [`finish_flow_run`]'s `status IN ('running','pending_approval')`
/// predicate exists to prevent. But a couple of tests legitimately need to
/// *stage* a row at an arbitrary terminal status (`completed_with_warnings`,
/// `interrupted`) to exercise the guards that read it, and they previously did
/// so by calling `finish_flow_run` twice — which the guard now correctly
/// refuses. Staging is a fixture concern, so it gets a fixture-only door rather
/// than a weaker production write.
#[cfg(any(test, feature = "test-fixtures"))]
pub fn force_run_status_for_test(
    dir: &Path,
    id: &str,
    status: &str,
    error: Option<&str>,
) -> Result<()> {
    with_connection(dir, |conn| {
        conn.execute(
            "UPDATE flow_runs SET status = ?1, error = ?2 WHERE id = ?3",
            params![status, error, id],
        )
        .context("Failed to force flow run status (test fixture)")?;
        Ok(())
    })
}

/// Test-only fixture door: overwrites an existing flow row's `graph_json`
/// with arbitrary text, bypassing the normal `Flow`/`WorkflowGraph`-typed
/// write path entirely. Used to stage the corrupt-or-newer-schema-row
/// scenario `list_flows` / `list_enabled_flows` / boot reconciliation must
/// survive (R-M4) — same "staging is a fixture concern, so it gets a
/// fixture-only door" rationale as [`force_run_status_for_test`]. Real
/// production writes can never produce a row `map_flow_row` can't decode
/// (every write path serializes a validated `WorkflowGraph`), so there is no
/// non-test way to reach this state other than a cross-version downgrade.
#[cfg(any(test, feature = "test-fixtures"))]
pub fn force_corrupt_graph_json_for_test(
    dir: &Path,
    flow_id: &str,
    raw_graph_json: &str,
) -> Result<()> {
    with_connection(dir, |conn| {
        let changed = conn
            .execute(
                "UPDATE flow_definitions SET graph_json = ?1 WHERE id = ?2",
                params![raw_graph_json, flow_id],
            )
            .context("Failed to force corrupt graph_json (test fixture)")?;
        anyhow::ensure!(changed > 0, "flow '{flow_id}' not found (test fixture)");
        Ok(())
    })
}

/// Flips a parked `'pending_approval'` row to `'running'` for the duration of a
/// `ops::flows_resume`, guarded by a
/// `status = 'pending_approval'` predicate so a run cancelled or expired
/// concurrently is never revived. Returns `true` when a row was actually
/// flipped.
///
/// Without this flip the row stays `pending_approval` for the whole (up to
/// `FLOW_RUN_TIMEOUT_SECS`) resume, so
/// [`expire_parked_runs`]' TTL sweep still matches it: a run approved just
/// before its TTL would be relabelled `cancelled` and have its durable
/// checkpoint dropped **while the resume was actively executing approved
/// outbound nodes** (R-M1). Marking it `running` moves it out of the sweep's
/// predicate and into the same lifecycle state a `flows_run` occupies, which is
/// also what the boot orphan sweep already knows how to reconcile.
pub fn mark_run_resuming(dir: &Path, id: &str) -> Result<bool> {
    with_connection(dir, |conn| {
        let changed = conn
            .execute(
                "UPDATE flow_runs SET status = 'running', finished_at = NULL, error = NULL \
                 WHERE id = ?1 AND status = 'pending_approval'",
                params![id],
            )
            .context("Failed to mark parked flow run as resuming")?;
        if changed > 0 {
            tracing::debug!(target: "flows", run_id = id, "[flows] marked parked run 'running' for the duration of the resume");
        }
        Ok(changed > 0)
    })
}

/// Reconciles a single orphaned `'running'` run row to a terminal
/// `'interrupted'` status stamped `now` (RFC3339) with `reason`, guarded by a
/// `status = 'running'` predicate so a run that settled or was resumed
/// concurrently is never clobbered. Returns `true` when a row was actually
/// flipped (bug B42 — cancellation-safe finalizer + boot sweep). Best-effort by
/// contract at the call site.
pub fn mark_run_interrupted(dir: &Path, id: &str, now: &str, reason: &str) -> Result<bool> {
    with_connection(dir, |conn| {
        let changed = conn
            .execute(
                "UPDATE flow_runs SET status = 'interrupted', finished_at = ?1, error = ?2 \
                 WHERE id = ?3 AND status = 'running'",
                params![now, reason, id],
            )
            .context("Failed to reconcile orphaned running flow run")?;
        if changed > 0 {
            tracing::info!(target: "flows", run_id = id, "[flows] reconciled orphaned 'running' flow run to 'interrupted'");
        }
        Ok(changed > 0)
    })
}

/// Loads one flow run by id (== thread_id).
pub fn get_flow_run(dir: &Path, id: &str) -> Result<Option<FlowRun>> {
    with_connection(dir, |conn| {
        let mut stmt = conn.prepare(&format!(
            "SELECT {FLOW_RUN_COLUMNS} FROM flow_runs WHERE id = ?1"
        ))?;
        let mut rows = stmt.query(params![id])?;
        match rows.next()? {
            Some(row) => Ok(Some(map_flow_run_row(row)?)),
            None => Ok(None),
        }
    })
}

/// Lists the most recent runs for a flow, newest first.
pub fn list_flow_runs(dir: &Path, flow_id: &str, limit: usize) -> Result<Vec<FlowRun>> {
    with_connection(dir, |conn| {
        let lim = i64::try_from(limit.max(1)).context("Run history limit overflow")?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {FLOW_RUN_COLUMNS} FROM flow_runs WHERE flow_id = ?1 \
             ORDER BY started_at DESC, id DESC LIMIT ?2"
        ))?;
        let rows = stmt.query_map(params![flow_id, lim], map_flow_run_row)?;
        let mut runs = Vec::new();
        for row in rows {
            runs.push(row?);
        }
        Ok(runs)
    })
}

/// List the most recent runs across ALL flows, newest first (the "All runs"
/// page). Uses the `idx_flow_runs_started_at` index for the ordering. Each
/// [`FlowRun`] carries its own `flow_id`, so the UI can group/label by flow.
pub fn list_all_flow_runs(dir: &Path, limit: usize) -> Result<Vec<FlowRun>> {
    with_connection(dir, |conn| {
        let lim = i64::try_from(limit.max(1)).context("Run history limit overflow")?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {FLOW_RUN_COLUMNS} FROM flow_runs \
             ORDER BY started_at DESC, id DESC LIMIT ?1"
        ))?;
        let rows = stmt.query_map(params![lim], map_flow_run_row)?;
        let mut runs = Vec::new();
        for row in rows {
            runs.push(row?);
        }
        Ok(runs)
    })
}

fn map_flow_run_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<FlowRun> {
    let steps_raw: String = row.get(6)?;
    let steps: Vec<FlowRunStep> = serde_json::from_str(&steps_raw).map_err(sql_conversion_error)?;
    let pending_raw: String = row.get(7)?;
    let pending_approvals: Vec<String> =
        serde_json::from_str(&pending_raw).map_err(sql_conversion_error)?;

    Ok(FlowRun {
        id: row.get(0)?,
        flow_id: row.get(1)?,
        thread_id: row.get(2)?,
        status: row.get(3)?,
        started_at: row.get(4)?,
        finished_at: row.get(5)?,
        steps,
        pending_approvals,
        error: row.get(8)?,
        graph_hash: row.get(9)?,
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// flow_suggestions — discovery-agent workflow suggestions (Flow Scout)
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "runs_tests.rs"]
mod tests;
