//! Jobs and runs to and from documents.
//!
//! Optional fields are left out of a document rather than stored as `null`;
//! nested values (`schedule`, `delivery`, `origin`) are JSON strings. A job
//! document that cannot be read back is an error naming the field, never a
//! silently defaulted job.

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use serde_json::{Map, Value, json};
use tinyflows_schedule::{
    CronJob, CronRun, DeliveryConfig, DeliveryStatus, JobOrigin, JobType, Schedule, SessionTarget,
};
use tinystoragedrivers_core::Versioned;

/// The string field `field` of `doc`.
pub(super) fn text<'a>(doc: &'a Value, field: &str) -> Option<&'a str> {
    doc.get(field).and_then(Value::as_str)
}

fn required<'a>(doc: &'a Value, field: &str) -> Result<&'a str> {
    optional(doc, field)?.ok_or_else(|| anyhow!("cron store: document has no `{field}`"))
}

/// The string field `field` of `doc`: `None` when absent, an error when
/// present with another type, so a malformed document is never read as a
/// default.
fn optional<'a>(doc: &'a Value, field: &str) -> Result<Option<&'a str>> {
    match doc.get(field) {
        None => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => Err(anyhow!("cron store: `{field}` is not a string")),
    }
}

/// A stored job type. Unknown values are an error rather than a shell job:
/// reading a tampered `job_type` as `shell` would run its `command`.
fn job_type(raw: &str) -> Result<JobType> {
    [JobType::Shell, JobType::Agent, JobType::Flow]
        .into_iter()
        .find(|known| known.as_str() == raw)
        .ok_or_else(|| anyhow!("cron store: unknown job type `{raw}`"))
}

/// The field holding a job's incarnation: a fresh id per created job, carried
/// by its runs, so a run written late for a removed job never attaches to a
/// later job that reuses its id (a flow's schedule job does).
pub(super) const INCARNATION: &str = "incarnation";

/// `doc`'s incarnation, when it has one.
pub(super) fn incarnation(doc: &Value) -> Option<&str> {
    text(doc, INCARNATION)
}

fn instant(raw: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(raw)
        .with_context(|| format!("Invalid RFC3339 timestamp in cron store: {raw}"))?
        .with_timezone(&Utc))
}

fn set(doc: &mut Map<String, Value>, field: &str, value: Option<Value>) {
    match value {
        Some(value) => {
            doc.insert(field.to_string(), value);
        }
        None => {
            doc.remove(field);
        }
    }
}

/// The document for `job`.
pub(super) fn job_to_doc(job: &CronJob) -> Result<Value> {
    let mut doc = Map::new();
    if !job.policy.is_default() {
        doc.insert("policy".into(), serde_json::to_value(job.policy)?);
    }
    doc.insert("expression".into(), json!(job.expression));
    doc.insert("command".into(), json!(job.command));
    doc.insert(
        "schedule".into(),
        json!(serde_json::to_string(&job.schedule).context("serialize cron schedule")?),
    );
    doc.insert("job_type".into(), json!(job.job_type.as_str()));
    doc.insert("session_target".into(), json!(job.session_target.as_str()));
    doc.insert("enabled".into(), json!(job.enabled));
    doc.insert(
        "delivery".into(),
        json!(serde_json::to_string(&job.delivery).context("serialize cron delivery")?),
    );
    doc.insert("delete_after_run".into(), json!(job.delete_after_run));
    doc.insert("created_at".into(), json!(job.created_at.to_rfc3339()));
    doc.insert(
        "created_ms".into(),
        json!(job.created_at.timestamp_millis()),
    );
    set_next_run(&mut doc, job.next_run);
    set(&mut doc, "prompt", job.prompt.as_ref().map(|v| json!(v)));
    set(&mut doc, "name", job.name.as_ref().map(|v| json!(v)));
    set(&mut doc, "model", job.model.as_ref().map(|v| json!(v)));
    set(
        &mut doc,
        "agent_id",
        job.agent_id.as_ref().map(|v| json!(v)),
    );
    set(
        &mut doc,
        "origin",
        job.origin
            .as_ref()
            .map(|origin| serde_json::to_string(origin).map(Value::String))
            .transpose()
            .context("Failed to serialize cron origin")?,
    );
    set(
        &mut doc,
        "last_run",
        job.last_run.map(|at| json!(at.to_rfc3339())),
    );
    set(
        &mut doc,
        "last_status",
        job.last_status.as_ref().map(|v| json!(v)),
    );
    set(
        &mut doc,
        "last_output",
        job.last_output.as_ref().map(|v| json!(v)),
    );
    Ok(Value::Object(doc))
}

/// Writes `next_run` in its readable form and its two ordering forms:
/// milliseconds (`next_run_ms`, kept so documents written before
/// `next_run_ns` still order and match) and nanoseconds (`next_run_ns`, so
/// two jobs due within one millisecond are picked in their real order).
pub(super) fn set_next_run(doc: &mut Map<String, Value>, next_run: DateTime<Utc>) {
    doc.insert("next_run".into(), json!(next_run.to_rfc3339()));
    doc.insert("next_run_ms".into(), json!(next_run.timestamp_millis()));
    doc.insert("next_run_ns".into(), json!(nanos(next_run)));
}

/// `at` in epoch nanoseconds, saturating outside 1677–2262: to `i64::MIN`
/// before that range and `i64::MAX` after it, so order is kept at both ends.
pub(super) fn nanos(at: DateTime<Utc>) -> i64 {
    at.timestamp_nanos_opt().unwrap_or(if at.timestamp() < 0 {
        i64::MIN
    } else {
        i64::MAX
    })
}

/// Records a run's outcome on a job document.
pub(super) fn set_last_run(
    doc: &mut Map<String, Value>,
    finished_at: DateTime<Utc>,
    success: bool,
    output: String,
) {
    doc.insert("last_run".into(), json!(finished_at.to_rfc3339()));
    doc.insert(
        "last_status".into(),
        json!(if success { "ok" } else { "error" }),
    );
    doc.insert("last_output".into(), json!(output));
}

/// The job a stored document holds.
pub(super) fn doc_to_job(stored: &Versioned<Value>) -> Result<CronJob> {
    let doc = &stored.doc;
    let schedule: Schedule = serde_json::from_str(required(doc, "schedule")?)
        .context("Failed to parse cron schedule JSON")?;
    let delivery: DeliveryConfig = match optional(doc, "delivery")? {
        Some(raw) => serde_json::from_str(raw).context("Failed to parse cron delivery JSON")?,
        None => DeliveryConfig::default(),
    };
    let origin: Option<JobOrigin> = optional(doc, "origin")?
        .map(serde_json::from_str)
        .transpose()
        .context("Failed to parse cron origin JSON")?;
    let flag = |field: &str| match doc.get(field) {
        None => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(anyhow!("cron store: `{field}` is not a boolean")),
    };
    let owned = |field: &str| optional(doc, field).map(|value| value.map(str::to_string));
    Ok(CronJob {
        policy: doc
            .get("policy")
            .map(|p| serde_json::from_value(p.clone()))
            .transpose()?
            .unwrap_or_default(),
        id: stored.id.clone(),
        expression: required(doc, "expression")?.to_string(),
        schedule,
        command: required(doc, "command")?.to_string(),
        prompt: owned("prompt")?,
        name: owned("name")?,
        job_type: job_type(required(doc, "job_type")?)?,
        session_target: SessionTarget::parse(
            optional(doc, "session_target")?.unwrap_or("isolated"),
        ),
        model: owned("model")?,
        agent_id: owned("agent_id")?,
        enabled: flag("enabled")?,
        delivery,
        delete_after_run: flag("delete_after_run")?,
        created_at: instant(required(doc, "created_at")?)?,
        next_run: instant(required(doc, "next_run")?)?,
        last_run: optional(doc, "last_run")?.map(instant).transpose()?,
        last_status: owned("last_status")?,
        last_output: owned("last_output")?,
        origin,
    })
}

/// The instant a job document was created, at full precision (for ordering
/// jobs created within one millisecond).
pub(super) fn created_at(doc: &Value) -> Option<DateTime<Utc>> {
    text(doc, "created_at").and_then(|raw| instant(raw).ok())
}

/// The document id of run number `seq`: zero-padded, so ids sort as numbers.
pub(super) fn run_id(seq: i64) -> String {
    format!("{seq:020}")
}

/// The document for one run.
#[allow(clippy::too_many_arguments)]
pub(super) fn run_to_doc(
    seq: i64,
    job_id: &str,
    incarnation: Option<&str>,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    status: &str,
    output: Option<String>,
    duration_ms: i64,
    delivery_status: Option<&DeliveryStatus>,
) -> Value {
    let mut doc = Map::new();
    doc.insert("seq".into(), json!(seq));
    doc.insert("job_id".into(), json!(job_id));
    doc.insert("started_at".into(), json!(started_at.to_rfc3339()));
    // Nanoseconds: two runs of one job can start within a millisecond, and
    // history order (and which run pruning keeps) follows the real start.
    doc.insert("started_ns".into(), json!(nanos(started_at)));
    doc.insert("finished_at".into(), json!(finished_at.to_rfc3339()));
    doc.insert("status".into(), json!(status));
    doc.insert("duration_ms".into(), json!(duration_ms));
    set(&mut doc, INCARNATION, incarnation.map(|value| json!(value)));
    set(&mut doc, "output", output.map(Value::String));
    set(
        &mut doc,
        "delivery_status",
        delivery_status.map(|status| json!(status.as_str())),
    );
    Value::Object(doc)
}

/// The run a stored document holds.
///
/// A field of the wrong type is an error naming it, never a default. A
/// delivery status string this build does not know reads as `None`, as it
/// does in the SQLite store.
pub(super) fn doc_to_run(stored: &Versioned<Value>) -> Result<CronRun> {
    let doc = &stored.doc;
    Ok(CronRun {
        id: integer(doc, "seq")?.ok_or_else(|| anyhow!("cron store: run document has no `seq`"))?,
        job_id: required(doc, "job_id")?.to_string(),
        started_at: instant(required(doc, "started_at")?)?,
        finished_at: instant(required(doc, "finished_at")?)?,
        status: required(doc, "status")?.to_string(),
        output: optional(doc, "output")?.map(str::to_string),
        duration_ms: integer(doc, "duration_ms")?,
        delivery_status: optional(doc, "delivery_status")?.and_then(DeliveryStatus::parse),
    })
}

/// The integer field `field` of `doc`: `None` when absent, an error when
/// present with another type.
pub(super) fn integer(doc: &Value, field: &str) -> Result<Option<i64>> {
    match doc.get(field) {
        None => Ok(None),
        Some(value) => value
            .as_i64()
            .map(Some)
            .ok_or_else(|| anyhow!("cron store: `{field}` is not an integer")),
    }
}

#[cfg(test)]
#[path = "codec_tests.rs"]
mod tests;
