//! Authoring suggestions: upsert, list, and status updates.

use anyhow::{Context, Result};
use rusqlite::params;
use std::path::Path;
use tinyflows_catalog::{FlowSuggestion, SuggestionStatus};

use super::{sql_conversion_error, with_connection};

/// Shared column list for every `flow_suggestions` SELECT — keeps
/// [`map_suggestion_row`]'s positional `row.get(N)` calls in sync with the query.
const FLOW_SUGGESTION_COLUMNS: &str = "id, title, one_liner, rationale, trigger_hint, steps_json, \
     connections_json, slugs_json, build_prompt, confidence, status, created_at, source_run_id";

/// Inserts a batch of freshly discovered suggestions.
///
/// **Dedupe-preserving upsert.** Each suggestion's `id` is a stable content
/// hash (see `discovery_tools`), so a re-run that re-proposes an identical idea
/// hits `ON CONFLICT(id)` and refreshes the *pitch* fields — **without**
/// resetting a `status` the user already set. This is the invariant that keeps a
/// dismissed idea dismissed and a built idea built across repeated discovery
/// runs: the `status` and `created_at` columns are deliberately excluded from
/// the `DO UPDATE SET` list. Returns the number of rows written.
pub fn upsert_suggestions(dir: &Path, suggestions: &[FlowSuggestion]) -> Result<usize> {
    if suggestions.is_empty() {
        return Ok(0);
    }
    with_connection(dir, |conn| {
        let mut written = 0usize;
        for s in suggestions {
            let steps_json = serde_json::to_string(&s.steps_outline)
                .context("Failed to serialize suggestion steps")?;
            let connections_json = serde_json::to_string(&s.suggested_connections)
                .context("Failed to serialize suggestion connections")?;
            let slugs_json = serde_json::to_string(&s.suggested_slugs)
                .context("Failed to serialize suggestion slugs")?;
            conn.execute(
                "INSERT INTO flow_suggestions
                    (id, title, one_liner, rationale, trigger_hint, steps_json,
                     connections_json, slugs_json, build_prompt, confidence, status,
                     created_at, source_run_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
                 ON CONFLICT(id) DO UPDATE SET
                    title = excluded.title,
                    one_liner = excluded.one_liner,
                    rationale = excluded.rationale,
                    trigger_hint = excluded.trigger_hint,
                    steps_json = excluded.steps_json,
                    connections_json = excluded.connections_json,
                    slugs_json = excluded.slugs_json,
                    build_prompt = excluded.build_prompt,
                    confidence = excluded.confidence,
                    source_run_id = excluded.source_run_id",
                params![
                    s.id,
                    s.title,
                    s.one_liner,
                    s.rationale,
                    s.trigger_hint,
                    steps_json,
                    connections_json,
                    slugs_json,
                    s.build_prompt,
                    s.confidence,
                    s.status.as_str(),
                    s.created_at,
                    s.source_run_id,
                ],
            )
            .context("Failed to upsert flow suggestion")?;
            written += 1;
        }
        tracing::debug!(count = written, "[flows] upserted flow suggestions");
        Ok(written)
    })
}

/// Lists persisted suggestions, newest first, highest-confidence first within a
/// timestamp. When `status` is `Some`, only rows in that lifecycle state are
/// returned (the UI passes `New` to render the active "Suggested for you"
/// cards); `None` returns every status.
pub fn list_suggestions(
    dir: &Path,
    status: Option<SuggestionStatus>,
    limit: usize,
) -> Result<Vec<FlowSuggestion>> {
    with_connection(dir, |conn| {
        let lim = i64::try_from(limit.max(1)).context("Suggestion limit overflow")?;
        let mut out = Vec::new();
        match status {
            Some(st) => {
                let mut stmt = conn.prepare(&format!(
                    "SELECT {FLOW_SUGGESTION_COLUMNS} FROM flow_suggestions WHERE status = ?1 \
                     ORDER BY created_at DESC, confidence DESC, id ASC LIMIT ?2"
                ))?;
                let rows = stmt.query_map(params![st.as_str(), lim], map_suggestion_row)?;
                for row in rows {
                    out.push(row?);
                }
            }
            None => {
                let mut stmt = conn.prepare(&format!(
                    "SELECT {FLOW_SUGGESTION_COLUMNS} FROM flow_suggestions \
                     ORDER BY created_at DESC, confidence DESC, id ASC LIMIT ?1"
                ))?;
                let rows = stmt.query_map(params![lim], map_suggestion_row)?;
                for row in rows {
                    out.push(row?);
                }
            }
        }
        Ok(out)
    })
}

/// Updates one suggestion's lifecycle status (dismiss / mark built). Returns
/// `true` when a row matched, `false` when the id was unknown (already pruned).
pub fn set_suggestion_status(dir: &Path, id: &str, status: SuggestionStatus) -> Result<bool> {
    with_connection(dir, |conn| {
        let changed = conn
            .execute(
                "UPDATE flow_suggestions SET status = ?1 WHERE id = ?2",
                params![status.as_str(), id],
            )
            .context("Failed to update flow suggestion status")?;
        tracing::debug!(suggestion_id = %id, status = %status.as_str(), changed, "[flows] set suggestion status");
        Ok(changed > 0)
    })
}

fn map_suggestion_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<FlowSuggestion> {
    let steps_raw: String = row.get(5)?;
    let steps_outline: Vec<String> =
        serde_json::from_str(&steps_raw).map_err(sql_conversion_error)?;
    let connections_raw: String = row.get(6)?;
    let suggested_connections: Vec<String> =
        serde_json::from_str(&connections_raw).map_err(sql_conversion_error)?;
    let slugs_raw: String = row.get(7)?;
    let suggested_slugs: Vec<String> =
        serde_json::from_str(&slugs_raw).map_err(sql_conversion_error)?;
    let status_raw: String = row.get(10)?;

    Ok(FlowSuggestion {
        id: row.get(0)?,
        title: row.get(1)?,
        one_liner: row.get(2)?,
        rationale: row.get(3)?,
        trigger_hint: row.get(4)?,
        steps_outline,
        suggested_connections,
        suggested_slugs,
        build_prompt: row.get(8)?,
        confidence: row.get(9)?,
        status: SuggestionStatus::from_str_lossy(&status_raw),
        created_at: row.get(11)?,
        source_run_id: row.get(12)?,
    })
}

#[cfg(test)]
#[path = "suggestions_tests.rs"]
mod tests;
