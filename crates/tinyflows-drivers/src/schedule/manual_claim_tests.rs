use super::test_support::docs_in;
use super::*;
use chrono::{Duration, Utc};
use tinyflows_schedule::{CronJobPatch, DeliveryStatus, Schedule};
use tinystoragedrivers_core::MemoryStorage;

#[tokio::test]
async fn reconciles_manual_claim_receipt_after_an_ambiguous_claim_reply() {
    let backend = MemoryStorage::new();
    let store = CronDocuments::new(docs_in(&backend, "manual-claim-reconcile"));
    let now = Utc::now();
    let job = store
        .add_shell_job(
            None,
            Schedule::At {
                at: now + Duration::hours(1),
            },
            "echo reconcile",
        )
        .await
        .unwrap();

    // Model a durable claim whose response was lost: only the preselected
    // owner survives at the call site, so it must be enough to recover receipt.
    let committed = store
        .claim_job_now(&job, "ambiguous-owner", now, Duration::minutes(2))
        .await
        .unwrap()
        .unwrap();
    assert_ne!(committed.owner(), committed.token());
    let committed_doc = store.docs.get("cron.jobs", &job.id).await.unwrap().unwrap();
    assert_eq!(committed_doc.doc["claim_owner"], "ambiguous-owner");
    assert!(committed_doc.doc.get("claim_epoch").is_some());
    assert!(
        store
            .reconcile_manual_claim_admission(&job, "other-owner", now + Duration::seconds(1))
            .await
            .unwrap()
            .is_none()
    );
    let after_other_owner_reconcile = store.docs.get("cron.jobs", &job.id).await.unwrap().unwrap();
    assert_eq!(
        after_other_owner_reconcile.doc["claim_epoch"], committed_doc.doc["claim_epoch"],
        "a different owner cannot change a committed lease receipt"
    );
    assert_eq!(
        after_other_owner_reconcile.doc["claim_owner"],
        "ambiguous-owner"
    );
    assert_eq!(
        store.get_job(&job.id).await.unwrap().claim_generation,
        job.claim_generation,
        "a live lease owned by someone else is left untouched"
    );
    assert!(
        store
            .renew_claim(&committed, now + Duration::seconds(1), Duration::minutes(1))
            .await
            .unwrap()
    );
    let reconciled = store
        .reconcile_manual_claim_admission(&job, "ambiguous-owner", now + Duration::seconds(1))
        .await
        .unwrap()
        .expect("a live claim for the unique owner is discoverable");
    assert_eq!(reconciled.owner(), committed.owner());
    assert_eq!(reconciled.token(), committed.token());
    assert!(
        store
            .release_claim(&reconciled, now + Duration::seconds(1))
            .await
            .unwrap()
    );
    assert!(
        store
            .reconcile_manual_claim_admission(&job, "ambiguous-owner", now + Duration::seconds(2))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn forced_manual_claim_queues_before_ack_and_replaces_queue_atomically() {
    let backend = MemoryStorage::new();
    let store = CronDocuments::new(docs_in(&backend, "manual-claim"));
    let job = store
        .add_shell_job(
            None,
            Schedule::At {
                at: Utc::now() + Duration::hours(1),
            },
            "echo manual",
        )
        .await
        .unwrap();
    store
        .update_job(
            &job.id,
            CronJobPatch {
                enabled: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let now = Utc::now();

    assert!(
        store
            .claim_due_job(&job.id, "scheduled", now, Duration::minutes(1))
            .await
            .unwrap()
            .is_none(),
        "scheduled admission must continue to reject disabled future jobs"
    );
    let claim = store
        .claim_job_now(&job, "manual-owner", now, Duration::minutes(1))
        .await
        .unwrap()
        .expect("manual run may force the saved disabled future job");
    assert_eq!(claim.job().id, job.id);
    assert!(
        store
            .claim_job_now(&job, "second-manual", now, Duration::minutes(1))
            .await
            .unwrap()
            .is_none()
    );
    assert!(store.queue_claimed_run(&claim, now).await.unwrap());
    let queued = store.list_runs(&job.id, 10).await.unwrap();
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].status, "queued");

    assert!(
        store
            .complete_claim(
                &claim,
                now + Duration::seconds(1),
                true,
                "ran once",
                Some(DeliveryStatus::NotRequested),
            )
            .await
            .unwrap()
    );
    let history = store.list_runs(&job.id, 10).await.unwrap();
    assert_eq!(
        history.len(),
        1,
        "the queue placeholder is replaced at terminal commit"
    );
    assert_eq!(history[0].status, "ok");
    assert_eq!(history[0].output.as_deref(), Some("ran once"));
    assert!(!store.get_job(&job.id).await.unwrap().enabled);

    let cancelled_at = now + Duration::seconds(2);
    let cancelled = store
        .claim_job_now(&job, "cancelled-manual", cancelled_at, Duration::minutes(1))
        .await
        .unwrap()
        .unwrap();
    assert!(
        store
            .queue_claimed_run(&cancelled, cancelled_at)
            .await
            .unwrap()
    );
    assert!(store.release_claim(&cancelled, cancelled_at).await.unwrap());
    let history = store.list_runs(&job.id, 10).await.unwrap();
    assert_eq!(history.len(), 1, "release removes its queued placeholder");
    assert_eq!(history[0].status, "ok");
}

#[tokio::test]
async fn expired_manual_recovery_removes_the_old_queued_placeholder() {
    let backend = MemoryStorage::new();
    let store = CronDocuments::new(docs_in(&backend, "manual-recovery-queue"));
    let job = store
        .add_shell_job(None, Schedule::Every { every_ms: 60_000 }, "echo recover")
        .await
        .unwrap();
    let started = Utc::now();
    let old = store
        .claim_job_now(&job, "crashed-owner", started, Duration::minutes(1))
        .await
        .unwrap()
        .unwrap();
    assert!(store.queue_claimed_run(&old, started).await.unwrap());
    assert_eq!(
        store.list_runs(&job.id, 10).await.unwrap()[0].status,
        "queued"
    );

    let recovered_at = started + Duration::minutes(2);
    let recovered = store
        .claim_job_now(
            &job,
            "replacement-owner",
            recovered_at,
            Duration::minutes(1),
        )
        .await
        .unwrap()
        .unwrap();
    assert_ne!(old.token(), recovered.token());
    assert!(
        store.list_runs(&job.id, 10).await.unwrap().is_empty(),
        "expired queued history is removed atomically with replacement claim"
    );
    assert!(
        store
            .queue_claimed_run(&recovered, recovered_at)
            .await
            .unwrap()
    );
    assert!(
        store
            .complete_manual_claim(
                &recovered,
                recovered_at + Duration::seconds(1),
                true,
                "recovered",
                None,
            )
            .await
            .unwrap()
    );
    let completed_doc = store.docs.get("cron.jobs", &job.id).await.unwrap().unwrap();
    assert!(completed_doc.doc.get("claim_epoch").is_none());
    assert!(completed_doc.doc.get("claim_owner").is_none());
    let history = store.list_runs(&job.id, 10).await.unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].status, "ok");
}

#[tokio::test]
async fn editing_a_queued_job_cleans_history_and_fences_late_completion() {
    let backend = MemoryStorage::new();
    let store = CronDocuments::new(docs_in(&backend, "manual-edit-fence"));
    let job = store
        .add_shell_job(
            None,
            Schedule::Every { every_ms: 60_000 },
            "echo before edit",
        )
        .await
        .unwrap();
    let started = Utc::now();
    let claim = store
        .claim_job_now(&job, "manual-owner", started, Duration::minutes(1))
        .await
        .unwrap()
        .unwrap();
    assert!(store.queue_claimed_run(&claim, started).await.unwrap());

    let updated = store
        .update_job(
            &job.id,
            CronJobPatch {
                name: Some("edited while queued".into()),
                command: Some("echo after edit".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.name.as_deref(), Some("edited while queued"));
    assert_eq!(updated.command, "echo after edit");
    assert!(store.list_runs(&job.id, 10).await.unwrap().is_empty());
    assert!(
        !store
            .complete_manual_claim(
                &claim,
                started + Duration::seconds(1),
                true,
                "late old result",
                None,
            )
            .await
            .unwrap()
    );
    let after_late_result = store.get_job(&job.id).await.unwrap();
    assert_eq!(
        after_late_result.name.as_deref(),
        Some("edited while queued")
    );
    assert_eq!(after_late_result.command, "echo after edit");
    assert!(after_late_result.last_run.is_none());
}
