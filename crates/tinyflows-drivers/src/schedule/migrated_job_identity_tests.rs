//! Schedule job edits preserve imported creation identity.

use super::super::test_support::{daily, store};
use super::*;
use serde_json::json;

#[tokio::test]
async fn editing_a_migrated_job_keeps_its_body_creation_identity() {
    let store = store();
    let job = store.add_shell_job(None, daily(), "work").await.unwrap();
    let created_at = chrono::DateTime::from_timestamp_millis(1_772_600_767_123_i64).unwrap();
    let stored = store.docs.get(JOBS, &job.id).await.unwrap().unwrap();
    let mut doc = stored.doc.as_object().cloned().unwrap();
    doc.insert("created_ms".into(), json!(created_at.timestamp_millis()));
    store
        .docs
        .put(JOBS, &job.id, Value::Object(doc), stored.unchanged())
        .await
        .unwrap();

    store
        .update_job(
            &job.id,
            tinyflows_schedule::CronJobPatch {
                command: Some("updated".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let started_at = Utc::now();
    store
        .record_claimed_run(&tinyflows_schedule::runtime::RunRecord {
            run_id: "migrated-job-edit-run".into(),
            job_id: job.id,
            job_created_at: created_at,
            started_at,
            finished_at: started_at,
            status: "ok".into(),
            output: "done".into(),
            delivery_status: None,
        })
        .await
        .expect("editing must preserve the imported job creation identity");
}
