use super::*;

use tinystoragedrivers_core::Version;

fn stored(doc: Value) -> Versioned<Value> {
    Versioned {
        id: "x".into(),
        version: Version::FIRST,
        doc,
    }
}

fn job() -> CronJob {
    let now = Utc::now();
    CronJob {
        policy: Default::default(),
        id: "x".into(),
        expression: "0 9 * * *".into(),
        schedule: Schedule::Cron {
            expr: "0 9 * * *".into(),
            tz: None,
            active_hours: None,
        },
        command: "ls".into(),
        prompt: None,
        name: None,
        job_type: JobType::Shell,
        session_target: SessionTarget::Isolated,
        model: None,
        agent_id: None,
        enabled: true,
        delivery: DeliveryConfig::default(),
        delete_after_run: false,
        created_at: now,
        next_run: now,
        last_run: None,
        last_status: None,
        last_output: None,
        origin: None,
    }
}

#[test]
fn absent_optionals_are_left_out_not_null() {
    let doc = job_to_doc(&job()).unwrap();
    for field in ["prompt", "name", "model", "agent_id", "origin", "last_run"] {
        assert!(doc.get(field).is_none(), "{field}");
    }
    assert!(doc["schedule"].is_string() && doc["delivery"].is_string());
    let read = doc_to_job(&stored(doc)).unwrap();
    assert_eq!(read.command, "ls");
    assert_eq!(read.delivery, DeliveryConfig::default());
}

#[test]
fn a_missing_delivery_reads_as_the_default() {
    let mut doc = job_to_doc(&job()).unwrap();
    doc.as_object_mut().unwrap().remove("delivery");
    assert_eq!(
        doc_to_job(&stored(doc)).unwrap().delivery,
        DeliveryConfig::default()
    );
}

#[test]
fn malformed_fields_are_errors() {
    let mut doc = job_to_doc(&job()).unwrap();
    doc["origin"] = json!("{not json");
    assert!(doc_to_job(&stored(doc)).is_err());
    let mut doc = job_to_doc(&job()).unwrap();
    doc["next_run"] = json!("yesterday");
    assert!(doc_to_job(&stored(doc)).is_err());
    let run = run_to_doc(1, "j", None, Utc::now(), Utc::now(), "ok", None, 1, None);
    let mut broken = run.clone();
    broken.as_object_mut().unwrap().remove("seq");
    assert!(doc_to_run(&stored(broken)).is_err());
    assert_eq!(doc_to_run(&stored(run)).unwrap().id, 1);
}

#[test]
fn run_ids_sort_as_numbers() {
    assert!(run_id(9) < run_id(10));
    assert_eq!(run_id(1).len(), 20);
}

#[test]
fn wrong_types_and_unknown_job_types_are_errors() {
    for (field, value) in [
        ("job_type", json!("bogus")),
        ("delivery", json!({})),
        ("origin", json!(7)),
        ("name", json!(1)),
        ("enabled", json!("yes")),
    ] {
        let mut doc = job_to_doc(&job()).unwrap();
        doc[field] = value;
        assert!(doc_to_job(&stored(doc)).is_err(), "{field}");
    }
    for field in ["expression", "command"] {
        let mut doc = job_to_doc(&job()).unwrap();
        doc.as_object_mut().unwrap().remove(field);
        assert!(doc_to_job(&stored(doc)).is_err(), "{field}");
    }
}

#[test]
fn creation_time_keeps_sub_millisecond_precision() {
    let mut early = job();
    early.created_at = "2026-01-01T00:00:00.000100Z".parse().unwrap();
    let mut late = job();
    late.created_at = "2026-01-01T00:00:00.000900Z".parse().unwrap();
    let early_doc = job_to_doc(&early).unwrap();
    let late_doc = job_to_doc(&late).unwrap();
    assert_eq!(early_doc["created_ms"], late_doc["created_ms"]);
    assert!(created_at(&early_doc) < created_at(&late_doc));
}

#[test]
fn wrong_typed_run_fields_are_errors() {
    let good = json!({
        "seq": 1, "job_id": "j", "started_at": "2026-01-01T00:00:00Z",
        "finished_at": "2026-01-01T00:00:01Z", "status": "ok",
    });
    assert!(doc_to_run(&stored(good.clone())).is_ok());
    for (field, bad) in [
        ("output", json!(7)),
        ("duration_ms", json!("12")),
        ("delivery_status", json!(true)),
        ("seq", json!("1")),
    ] {
        let mut doc = good.clone();
        doc[field] = bad;
        let error = doc_to_run(&stored(doc)).unwrap_err().to_string();
        assert!(error.contains(field), "{field}: {error}");
    }
    let mut unknown = good;
    unknown["delivery_status"] = json!("teleported");
    assert!(
        doc_to_run(&stored(unknown))
            .unwrap()
            .delivery_status
            .is_none()
    );
}

#[test]
fn next_run_is_written_at_both_precisions() {
    let mut doc = serde_json::Map::new();
    let at = DateTime::parse_from_rfc3339("2026-01-01T00:00:00.000000123Z")
        .unwrap()
        .with_timezone(&Utc);
    set_next_run(&mut doc, at);
    assert_eq!(doc["next_run_ms"], json!(at.timestamp_millis()));
    assert_eq!(
        doc["next_run_ns"],
        json!(at.timestamp_millis() * 1_000_000 + 123)
    );
}

#[test]
fn nanoseconds_saturate_toward_the_side_they_overflow() {
    let early = DateTime::parse_from_rfc3339("1600-01-01T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let late = DateTime::parse_from_rfc3339("2300-01-01T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    assert_eq!(nanos(early), i64::MIN);
    assert_eq!(nanos(late), i64::MAX);
    assert!(nanos(early) < nanos(Utc::now()));
}
