//! The catalog's schema v2 declarations and its v1 -> v2 steps.
//!
//! Every step is a pure function of one v1 record: it renames the collection
//! (`flows_*` -> `flows.*`), turns the JSON strings (`graph_json`,
//! `steps_json`, ...) into the values they hold, replaces each RFC 3339 /
//! nanosecond pair with one `_ms` field and drops the `*_ns` copies. Ids are
//! unchanged. A JSON string that does not parse is carried as the string, so a
//! corrupt record stays corrupt (and visible) instead of being lost.

use serde_json::{Map, Value};
use tinystoragedrivers_core::{Schema, ScopeClass, SourceRecord, Step, TargetRecord};

use super::{
    DEFINITIONS, DRAFTS, FlowCatalogDocuments, REVISIONS, RUNS, SCHEMA_VERSION, SUGGESTIONS,
    instant_ms,
};

/// Declares the catalog collections on `schema` (owner `flows`) with their steps.
pub(crate) fn extend_schema(mut schema: Schema) -> Schema {
    for spec in FlowCatalogDocuments::collections() {
        schema = schema.collection_with(SCHEMA_VERSION, spec, ScopeClass::Tenant);
    }
    schema
        .step(
            Step::new("definitions_v2", DEFINITIONS, 1, 2, |batch| {
                Ok(map(batch, DEFINITIONS, definition))
            })
            .source("flows_definitions"),
        )
        .step(
            Step::new("revisions_v2", REVISIONS, 1, 2, |batch| {
                Ok(map(batch, REVISIONS, revision))
            })
            .source("flows_revisions"),
        )
        .step(
            Step::new("runs_v2", RUNS, 1, 2, |batch| Ok(map(batch, RUNS, run)))
                .source("flows_runs"),
        )
        .step(
            Step::new("suggestions_v2", SUGGESTIONS, 1, 2, |batch| {
                Ok(map(batch, SUGGESTIONS, suggestion))
            })
            .source("flows_suggestions"),
        )
        .step(
            Step::new("drafts_v2", DRAFTS, 1, 2, |batch| {
                Ok(map(batch, DRAFTS, draft))
            })
            .source("flows_drafts"),
        )
}

type Convert = fn(&SourceRecord) -> Option<(Value, Option<i64>)>;

/// Converts each record. One that does not convert (a missing JSON field, a
/// body that is not an object) is carried unchanged under its v1 id, so it
/// stays visible as corrupt instead of being lost with its v1 source.
fn map(batch: Vec<SourceRecord>, collection: &str, convert: Convert) -> Vec<TargetRecord> {
    batch
        .iter()
        .map(|source| {
            let Some((body, created_ms)) = convert(source) else {
                tracing::warn!(collection, id = %source.id, "[flows-storage] carrying unconvertible v1 record");
                return TargetRecord::new(collection, source.id.clone(), source.body.clone());
            };
            let record = TargetRecord::new(collection, source.id.clone(), body);
            match created_ms {
                // Zero and negative values are valid dates, not an unknown-metadata sentinel.
                Some(ms) => record.created_ms(ms),
                None => record,
            }
        })
        .collect()
}

/// A JSON string parsed into the value it holds; the string itself when it
/// does not parse.
fn parsed(raw: &Value) -> Value {
    raw.as_str()
        .and_then(|text| serde_json::from_str(text).ok())
        .unwrap_or_else(|| raw.clone())
}

/// The instant of a v1 record in milliseconds: the RFC 3339 field, else the
/// nanosecond field divided down.
fn ms(body: &Value, rfc: &str, ns: &str) -> Option<i64> {
    body.get(rfc)
        .and_then(Value::as_str)
        .map(instant_ms)
        .or_else(|| body.get(ns).and_then(Value::as_i64).map(|n| n / 1_000_000))
}

/// Copies `fields` verbatim from `body` into `out`.
fn carry(body: &Value, out: &mut Map<String, Value>, fields: &[&str]) {
    for field in fields {
        if let Some(value) = body.get(*field) {
            out.insert((*field).to_owned(), value.clone());
        }
    }
}

fn definition(source: &SourceRecord) -> Option<(Value, Option<i64>)> {
    let body = &source.body;
    let mut out = Map::new();
    carry(
        body,
        &mut out,
        &[
            "name",
            "enabled",
            "require_approval",
            "last_revision_id",
            "revision",
            "incarnation",
            "last_status",
        ],
    );
    out.insert("graph".into(), parsed(body.get("graph_json")?));
    // A parsed body timestamp (including zero or pre-epoch) wins; otherwise retain driver metadata.
    let created = ms(body, "created_at", "created_ns").unwrap_or(source.meta.created_ms);
    out.insert("created_ms".into(), created.into());
    out.insert(
        "updated_ms".into(),
        ms(body, "updated_at", "updated_ns")
            .unwrap_or(created)
            .into(),
    );
    if let Some(last) = ms(body, "last_run_at", "last_run_ns") {
        out.insert("last_run_ms".into(), last.into());
    }
    Some((Value::Object(out), Some(created)))
}

fn revision(source: &SourceRecord) -> Option<(Value, Option<i64>)> {
    let body = &source.body;
    let mut out = Map::new();
    carry(
        body,
        &mut out,
        &[
            "flow_id",
            "name",
            "require_approval",
            "pending",
            "flow_incarnation",
        ],
    );
    out.insert("graph".into(), parsed(body.get("graph_json")?));
    let created = ms(body, "created_at", "created_ns").unwrap_or(source.meta.created_ms);
    out.insert("created_ms".into(), created.into());
    if let Some(touched) = body.get("touched_ns").and_then(Value::as_i64) {
        out.insert("touched_ms".into(), (touched / 1_000_000).into());
    }
    Some((Value::Object(out), Some(created)))
}

fn run(source: &SourceRecord) -> Option<(Value, Option<i64>)> {
    let body = &source.body;
    let mut out = Map::new();
    carry(
        body,
        &mut out,
        &[
            "flow_id",
            "thread_id",
            "status",
            "error",
            "graph_hash",
            "flow_incarnation",
            "execution_snapshot",
            "terminal_event_seq",
        ],
    );
    let started = ms(body, "started_at", "started_ns").unwrap_or(source.meta.created_ms);
    out.insert("started_ms".into(), started.into());
    if let Some(finished) = ms(body, "finished_at", "finished_ns") {
        out.insert("finished_ms".into(), finished.into());
    }
    out.insert(
        "steps".into(),
        body.get("steps_json")
            .map_or_else(|| Value::Array(vec![]), parsed),
    );
    out.insert(
        "pending_approvals".into(),
        body.get("pending_approvals_json")
            .map_or_else(|| Value::Array(vec![]), parsed),
    );
    // The event stream exists only on a run that appended one.
    if let Some(events) = body.get("events_json") {
        out.insert("events".into(), parsed(events));
    }
    Some((Value::Object(out), Some(started)))
}

fn suggestion(source: &SourceRecord) -> Option<(Value, Option<i64>)> {
    let body = &source.body;
    let mut out = Map::new();
    carry(
        body,
        &mut out,
        &[
            "title",
            "one_liner",
            "rationale",
            "trigger_hint",
            "build_prompt",
            "confidence",
            "status",
            "source_run_id",
        ],
    );
    for (v1, v2) in [
        ("steps_json", "steps_outline"),
        ("connections_json", "suggested_connections"),
        ("slugs_json", "suggested_slugs"),
    ] {
        out.insert(
            v2.into(),
            body.get(v1).map_or_else(|| Value::Array(vec![]), parsed),
        );
    }
    let created = ms(body, "created_at", "created_ns").unwrap_or(source.meta.created_ms);
    out.insert("created_ms".into(), created.into());
    Some((Value::Object(out), Some(created)))
}

fn draft(source: &SourceRecord) -> Option<(Value, Option<i64>)> {
    let whole = parsed(source.body.get("draft_json")?);
    let whole = whole.as_object()?;
    let mut out = Map::new();
    for field in ["flow_id", "name", "graph", "origin"] {
        if let Some(value) = whole.get(field) {
            out.insert(field.to_owned(), value.clone());
        }
    }
    let updated = whole
        .get("updated_at")
        .and_then(Value::as_str)
        .map(instant_ms)
        .or_else(|| {
            source
                .body
                .get("updated_ns")
                .and_then(Value::as_i64)
                .map(|n| n / 1_000_000)
        })
        .unwrap_or(source.meta.created_ms);
    // A missing embedded creation time falls back to the source record's own creation time.
    let created = whole
        .get("created_at")
        .and_then(Value::as_str)
        .map(instant_ms)
        .unwrap_or(source.meta.created_ms);
    out.insert("created_ms".into(), created.into());
    out.insert("updated_ms".into(), updated.into());
    Some((Value::Object(out), Some(created)))
}

#[cfg(test)]
#[path = "migrate_tests.rs"]
mod tests;
