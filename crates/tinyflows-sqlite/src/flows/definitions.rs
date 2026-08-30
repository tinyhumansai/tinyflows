//! `flow_definitions` CRUD: create, read, list, enable/disable, delete,
//! duplicate, and last-run bookkeeping (`record_run`).

use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{Connection, params};
use std::path::Path;
use tinyflows_catalog::Flow;
use uuid::Uuid;

use super::{sql_conversion_error, with_connection};

/// Shared column list for every `flow_definitions` SELECT — keeps
/// [`map_flow_row`]'s positional `row.get(N)` calls in sync with the query.
const FLOW_DEFINITION_COLUMNS: &str = "id, name, graph_json, enabled, created_at, updated_at, \
     last_run_at, last_status, require_approval";

/// Inserts or fully replaces a flow definition row.
pub fn upsert_flow(dir: &Path, flow: &Flow) -> Result<()> {
    let graph_json = serde_json::to_string(&flow.graph).context("Failed to serialize graph")?;
    with_connection(dir, |conn| {
        conn.execute(
            "INSERT INTO flow_definitions
                (id, name, graph_json, enabled, created_at, updated_at, last_run_at, last_status, require_approval)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(id) DO UPDATE SET
                name = excluded.name,
                graph_json = excluded.graph_json,
                enabled = excluded.enabled,
                updated_at = excluded.updated_at,
                last_run_at = excluded.last_run_at,
                last_status = excluded.last_status,
                require_approval = excluded.require_approval",
            params![
                flow.id,
                flow.name,
                graph_json,
                if flow.enabled { 1 } else { 0 },
                flow.created_at,
                flow.updated_at,
                flow.last_run_at,
                flow.last_status,
                if flow.require_approval { 1 } else { 0 },
            ],
        )
        .context("Failed to upsert flow definition")?;
        tracing::debug!(flow_id = %flow.id, "[flows] upserted flow definition");
        Ok(())
    })
}

/// Duplicates an existing [`Flow`] into a fresh row: same graph +
/// `require_approval`, a new id/timestamps, the given `new_name`, and
/// **`enabled = false`** so the copy never auto-fires (no schedule/app_event
/// trigger is bound while disabled — the caller relies on this to keep a
/// duplicate inert until explicitly enabled). `last_run_at`/`last_status` are
/// reset to `None` — run history does not carry over. Returns the persisted
/// copy.
pub fn insert_duplicate_flow(dir: &Path, source: &Flow, new_name: String) -> Result<Flow> {
    let now = Utc::now().to_rfc3339();
    let flow = Flow {
        id: Uuid::new_v4().to_string(),
        name: new_name,
        enabled: false,
        graph: source.graph.clone(),
        created_at: now.clone(),
        updated_at: now,
        last_run_at: None,
        last_status: None,
        require_approval: source.require_approval,
    };
    upsert_flow(dir, &flow)?;
    tracing::debug!(target: "flows", source_id = %source.id, new_id = %flow.id, "[flows] inserted duplicate flow (disabled)");
    Ok(flow)
}

/// Creates a brand-new [`Flow`] row from a name + validated graph, stamping
/// fresh id/timestamps, and returns the persisted record.
///
/// `enabled` is decided by the caller (`ops::flows_create`,
/// issue B29 — save/enable safety): a graph with an automatic trigger
/// (`schedule` / `app_event` / `webhook`) is created disabled so it cannot
/// silently arm itself live and unattended; a `manual`-triggered graph is
/// created enabled since it only ever runs on explicit `flows_run`.
pub fn create_flow(
    dir: &Path,
    name: String,
    graph: tinyflows::model::WorkflowGraph,
    require_approval: bool,
    enabled: bool,
) -> Result<Flow> {
    let now = Utc::now().to_rfc3339();
    let flow = Flow {
        id: Uuid::new_v4().to_string(),
        name,
        enabled,
        graph,
        created_at: now.clone(),
        updated_at: now,
        last_run_at: None,
        last_status: None,
        require_approval,
    };
    upsert_flow(dir, &flow)?;
    Ok(flow)
}

/// Loads one flow by id, running its stored `graph_json` through
/// `tinyflows::migrate::migrate` before deserializing so a graph persisted
/// under an older `schema_version` is upgraded on read.
pub fn get_flow(dir: &Path, id: &str) -> Result<Option<Flow>> {
    with_connection(dir, |conn| {
        let mut stmt = conn.prepare(&format!(
            "SELECT {FLOW_DEFINITION_COLUMNS} FROM flow_definitions WHERE id = ?1"
        ))?;
        let mut rows = stmt.query(params![id])?;
        match rows.next()? {
            Some(row) => Ok(Some(map_flow_row(row)?)),
            None => Ok(None),
        }
    })
}

/// Runs a `flow_definitions` SELECT and splits its rows into successfully
/// decoded [`Flow`]s and a count of rows that failed to parse/migrate
/// (R-M4).
///
/// **Skip-and-log, not fail-the-whole-query.** Before this, `list_flows` /
/// `list_enabled_flows` did `flows.push(row?)`, so a single corrupt or
/// newer-schema-than-this-build `graph_json` (e.g. a user downgrades after
/// running a newer build that persisted a graph `tinyflows::migrate::migrate`
/// cannot step backward) hard-failed the *entire* query — bricking every
/// `flows_list`, every `app_event` trigger dispatch (which is driven by
/// `list_enabled_flows`, see `bus.rs::handle_app_event`), and the boot
/// `reconcile_schedule_triggers_on_boot` sweep, all because of one bad row.
/// Mirrors the posture `draft_store::list_drafts` already uses. The returned
/// skip count is **not** swallowed here — it is the caller's job to log/
/// surface it loudly (a silently short flow list is its own failure mode) —
/// but this function itself does log each skip at `warn` with the row's `id`
/// and the parse/migrate error, never the `graph_json` payload.
fn list_flow_rows(conn: &Connection, where_clause: &str) -> Result<(Vec<Flow>, usize)> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {FLOW_DEFINITION_COLUMNS} FROM flow_definitions {where_clause} \
         ORDER BY created_at ASC"
    ))?;
    let mut rows = stmt.query([])?;
    let mut flows = Vec::new();
    let mut skipped = 0usize;
    while let Some(row) = rows.next()? {
        match map_flow_row(row) {
            Ok(flow) => flows.push(flow),
            Err(e) => {
                skipped += 1;
                let id: String = row.get(0).unwrap_or_else(|_| "<unknown>".to_string());
                tracing::warn!(
                    target: "flows",
                    flow_id = %id,
                    error = %e,
                    "[flows] skipping corrupt or unmigratable flow_definitions row \
                     (graph_json failed to parse/migrate)"
                );
            }
        }
    }
    Ok((flows, skipped))
}

/// Lists all saved flows, migrating each graph on read (see [`get_flow`]).
///
/// Returns `(flows, skipped)` — `skipped` is the number of rows that could
/// not be decoded and were left out of `flows` (R-M4). Callers must not treat
/// a non-zero `skipped` as a reason to fail; they must surface it loudly
/// instead (see [`list_flow_rows`]).
pub fn list_flows(dir: &Path) -> Result<(Vec<Flow>, usize)> {
    with_connection(dir, |conn| list_flow_rows(conn, ""))
}

/// Lists only enabled flows, migrating each graph on read (see [`get_flow`]).
///
/// Used by `flows::bus::FlowTriggerSubscriber` to match an inbound
/// `ComposioTriggerReceived` event against every enabled `app_event` flow —
/// scanning the (small) enabled set once per event is simpler and cheap
/// enough at expected flow counts; a dedicated toolkit/trigger_slug index is
/// a later optimization if this ever shows up as a bottleneck.
///
/// Returns `(flows, skipped)` — see [`list_flows`]. A corrupt row here must
/// not take down `app_event` dispatch for every *other* enabled flow (R-M4).
pub fn list_enabled_flows(dir: &Path) -> Result<(Vec<Flow>, usize)> {
    with_connection(dir, |conn| list_flow_rows(conn, "WHERE enabled = 1"))
}

/// Deletes a flow by id. Returns an error if no such flow exists.
pub fn remove_flow(dir: &Path, id: &str) -> Result<()> {
    let changed = with_connection(dir, |conn| {
        conn.execute("DELETE FROM flow_definitions WHERE id = ?1", params![id])
            .context("Failed to delete flow definition")
    })?;
    if changed == 0 {
        anyhow::bail!("flow '{id}' not found");
    }
    tracing::debug!(flow_id = %id, "[flows] removed flow definition");
    Ok(())
}

/// Toggles a flow's `enabled` flag, returning the updated record.
pub fn set_enabled(dir: &Path, id: &str, enabled: bool) -> Result<Flow> {
    let now = Utc::now().to_rfc3339();
    let changed = with_connection(dir, |conn| {
        conn.execute(
            "UPDATE flow_definitions SET enabled = ?1, updated_at = ?2 WHERE id = ?3",
            params![if enabled { 1 } else { 0 }, now, id],
        )
        .context("Failed to update flow enabled state")
    })?;
    if changed == 0 {
        anyhow::bail!("flow '{id}' not found");
    }
    tracing::debug!(flow_id = %id, enabled, "[flows] set_enabled");
    get_flow(dir, id)?.ok_or_else(|| anyhow::anyhow!("flow '{id}' not found after update"))
}

/// Records the outcome of a `flows_run` invocation onto the flow's summary
/// fields (`last_run_at` / `last_status`).
pub fn record_run(dir: &Path, id: &str, status: &str) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    let changed = with_connection(dir, |conn| {
        conn.execute(
            "UPDATE flow_definitions SET last_run_at = ?1, last_status = ?2 WHERE id = ?3",
            params![now, status, id],
        )
        .context("Failed to record flow run")
    })?;
    if changed == 0 {
        anyhow::bail!("flow '{id}' not found");
    }
    tracing::debug!(flow_id = %id, status, "[flows] recorded run");
    Ok(())
}

fn map_flow_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Flow> {
    let graph_raw: String = row.get(2)?;
    let raw_value: serde_json::Value =
        serde_json::from_str(&graph_raw).map_err(sql_conversion_error)?;
    let migrated = tinyflows::migrate::migrate(raw_value).map_err(sql_conversion_error)?;
    let graph: tinyflows::model::WorkflowGraph =
        serde_json::from_value(migrated).map_err(sql_conversion_error)?;

    Ok(Flow {
        id: row.get(0)?,
        name: row.get(1)?,
        graph,
        enabled: row.get::<_, i64>(3)? != 0,
        created_at: row.get(4)?,
        updated_at: row.get(5)?,
        last_run_at: row.get(6)?,
        last_status: row.get(7)?,
        require_approval: row.get::<_, i64>(8)? != 0,
    })
}

#[cfg(test)]
#[path = "definitions_tests.rs"]
mod tests;
