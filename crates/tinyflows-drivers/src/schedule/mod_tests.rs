use super::*;
use tinystoragedrivers_testkit::MemoryBackend as MemoryStorage;

use super::test_support::{daily, docs_in, store};
use chrono::Utc;
use serde_json::json;
use tinyflows_storage::Precondition;

async fn refuses_malformed_legacy_policy(policy: serde_json::Value) {
    use tinystoragedrivers_types::CollectionSpec;
    let store = store();
    let job = store
        .add_shell_job(Some("system".into()), daily(), "system:seeded")
        .await
        .unwrap();
    let before = store.docs.get(JOBS, &job.id).await.unwrap().unwrap();
    store
        .docs
        .ensure_collection(&CollectionSpec::new("cron_job_policies"))
        .await
        .unwrap();
    store
        .docs
        .put(
            "cron_job_policies",
            &job.id,
            policy.clone(),
            Precondition::Absent,
        )
        .await
        .unwrap();

    assert!(
        store.get_job(&job.id).await.is_err(),
        "malformed legacy policy must prevent scheduling: {policy}"
    );
    assert_eq!(
        store.docs.get(JOBS, &job.id).await.unwrap().unwrap().doc,
        before.doc,
        "refusing a policy must preserve the seeded job"
    );
    assert_eq!(
        store
            .docs
            .get("cron_job_policies", &job.id)
            .await
            .unwrap()
            .unwrap()
            .doc,
        policy,
        "an invalid policy must remain available for repair"
    );
}

#[tokio::test]
async fn malformed_legacy_retry_budgets_fail_closed_without_mutating_seeded_jobs() {
    for retries in [json!("2"), json!(-1), json!(u64::from(u32::MAX) + 1)] {
        refuses_malformed_legacy_policy(json!({"retries": retries, "single_flight": true})).await;
    }
}

#[tokio::test]
async fn malformed_legacy_single_flight_fails_closed_without_deleting_the_policy() {
    refuses_malformed_legacy_policy(json!({"retries": 0, "single_flight": "true"})).await;
    refuses_malformed_legacy_policy(json!({"retries": 0})).await;
}

#[test]
fn limits_default_like_the_sqlite_store_and_clamp_to_one() {
    let store = store();
    assert_eq!((store.max_run_history, store.max_tasks), (50, 64));
    let clamped = store.with_limits(0, 0);
    assert_eq!((clamped.max_run_history, clamped.max_tasks), (1, 1));
}

#[tokio::test]
async fn ensure_is_idempotent_and_declares_every_collection() {
    let store = store();
    store.ensure().await.unwrap();
    store.ensure().await.unwrap();
    let names: Vec<String> = CronDocuments::collections()
        .into_iter()
        .map(|spec| spec.name)
        .collect();
    assert_eq!(names, [JOBS, RUNS, ADMISSION]);
}

#[tokio::test]
async fn scopes_keep_jobs_and_runs_apart() {
    let storage = MemoryStorage::new();
    let alice = CronDocuments::new(docs_in(&storage, "alice"));
    let bob = CronDocuments::new(docs_in(&storage, "bob"));
    let job = alice.add_shell_job(None, daily(), "echo a").await.unwrap();
    let now = Utc::now();
    alice
        .record_run(&job.id, now, now, "ok", None, 1)
        .await
        .unwrap();
    assert!(bob.list_jobs().await.unwrap().is_empty());
    assert!(bob.get_job(&job.id).await.is_err());
    assert!(bob.list_runs(&job.id, 10).await.unwrap().is_empty());
    assert!(bob.remove_job(&job.id).await.is_err());
    assert_eq!(alice.list_jobs().await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_corrupt_job_document_is_an_error_naming_the_field() {
    let store = store();
    store.ensure().await.unwrap();
    store
        .docs
        .put(
            JOBS,
            "bad",
            json!({ "job_type": "shell" }),
            Precondition::Absent,
        )
        .await
        .unwrap();
    let error = store.get_job("bad").await.unwrap_err().to_string();
    assert!(error.contains("schedule"), "{error}");
    assert!(store.list_jobs().await.is_err());
}

#[tokio::test]
async fn compare_and_swap_skips_missing_documents_and_declined_changes() {
    let store = store();
    store.ensure().await.unwrap();
    assert!(
        compare_job(&store, JOBS, "missing", |_| Ok(Some(json!({}))))
            .await
            .unwrap()
            .is_none()
    );
    store
        .docs
        .put(JOBS, "j", json!({ "n": 1 }), Precondition::Absent)
        .await
        .unwrap();
    assert!(
        compare_job(&store, JOBS, "j", |_| Ok(None))
            .await
            .unwrap()
            .is_none()
    );
    let error = compare_job(&store, JOBS, "j", |_| anyhow::bail!("refused"))
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "refused");
}

#[tokio::test]
async fn legacy_policy_documents_move_into_jobs_without_losing_seeded_fields() {
    use tinyflows_storage::CollectionSpec;
    let store = store();
    let job = store
        .add_shell_job(Some("system".into()), daily(), "system:seeded")
        .await
        .unwrap();
    let before = store.docs.get(JOBS, &job.id).await.unwrap().unwrap();
    store
        .docs
        .ensure_collection(&CollectionSpec::new("cron_job_policies"))
        .await
        .unwrap();
    store
        .docs
        .put(
            "cron_job_policies",
            &job.id,
            json!({"retries": 0, "single_flight": true}),
            Precondition::Absent,
        )
        .await
        .unwrap();
    let _ = store.get_job(&job.id).await.unwrap();
    let mut after = store.docs.get(JOBS, &job.id).await.unwrap().unwrap().doc;
    let policy = after
        .as_object_mut()
        .unwrap()
        .remove("policy")
        .unwrap_or_default();
    assert_eq!(policy, json!({"retries": 0, "single_flight": true}));
    assert_eq!(after, before.doc);
    assert!(
        store
            .docs
            .get("cron_job_policies", &job.id)
            .await
            .unwrap()
            .is_none()
    );
}
