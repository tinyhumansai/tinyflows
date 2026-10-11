use super::*;

use super::super::test_support::store;
use tinyflows_schedule::{CronJobPatch, Schedule};
#[tokio::test]
async fn completion_keeps_a_schedule_edited_after_the_slot_was_claimed() {
    let store = store();
    let job = store
        .add_shell_job(
            Some("seed".into()),
            Schedule::Every { every_ms: 60000 },
            "echo seed",
        )
        .await
        .unwrap();
    assert!(
        store
            .claim_slot(&job, job.next_run + chrono::Duration::seconds(60))
            .await
            .unwrap()
    );
    assert!(
        !store
            .claim_slot(&job, job.next_run + chrono::Duration::seconds(60))
            .await
            .unwrap()
    );
    let edited = store
        .update_job(
            &job.id,
            CronJobPatch {
                schedule: Some(Schedule::Every { every_ms: 300000 }),
                enabled: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    store
        .complete_run(
            &job,
            &Completion {
                remove: false,
                disable: false,
                next_run: Some(job.next_run + chrono::Duration::hours(1)),
                finished_at: Utc::now(),
                success: true,
                output: "finished".into(),
            },
        )
        .await
        .unwrap();
    let kept = store.get_job(&job.id).await.unwrap();
    assert_eq!(kept.schedule, edited.schedule);
    assert_eq!(kept.next_run, edited.next_run);
    assert!(!kept.enabled);
    assert_eq!(kept.last_output.as_deref(), Some("finished"));
}
#[tokio::test]
async fn completed_one_shot_is_disabled_and_successful_auto_delete_is_idempotent() {
    let store = store();
    for remove in [false, true] {
        let job = store
            .add_shell_job(
                None,
                Schedule::At {
                    at: Utc::now() + chrono::Duration::hours(1),
                },
                "echo once",
            )
            .await
            .unwrap();
        let job = store
            .update_job(
                &job.id,
                CronJobPatch {
                    delete_after_run: Some(remove),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(store.claim_slot(&job, job.next_run).await.unwrap());
        let completion = Completion {
            remove,
            disable: !remove,
            next_run: None,
            finished_at: Utc::now(),
            success: true,
            output: "done".into(),
        };
        store.complete_run(&job, &completion).await.unwrap();
        store.complete_run(&job, &completion).await.unwrap();
        if remove {
            assert!(store.get_job(&job.id).await.is_err());
        } else {
            assert!(!store.get_job(&job.id).await.unwrap().enabled);
        }
    }
}

#[tokio::test]
async fn completing_a_one_shot_releases_capacity_under_installed_admission_policy() {
    for remove in [false, true] {
        let store = store();
        store.set_max_active_jobs(Some(1)).await.unwrap();
        let job = store
            .add_shell_job(
                None,
                Schedule::At {
                    at: Utc::now() + chrono::Duration::hours(1),
                },
                "echo once",
            )
            .await
            .unwrap();
        assert!(
            store
                .add_shell_job(None, Schedule::Every { every_ms: 60_000 }, "blocked")
                .await
                .is_err()
        );
        let job = store
            .update_job(
                &job.id,
                CronJobPatch {
                    delete_after_run: Some(remove),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(store.claim_slot(&job, job.next_run).await.unwrap());
        let completion = Completion {
            remove,
            disable: !remove,
            next_run: None,
            finished_at: Utc::now(),
            success: true,
            output: "done".into(),
        };
        store.complete_run(&job, &completion).await.unwrap();
        store.complete_run(&job, &completion).await.unwrap();
        let next = store
            .add_shell_job(None, Schedule::Every { every_ms: 60_000 }, "next")
            .await
            .unwrap();
        assert!(next.enabled);
        assert!(
            store
                .add_shell_job(None, Schedule::Every { every_ms: 60_000 }, "still blocked")
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn one_shot_claim_is_durable_without_history_or_completion() {
    let store = store();
    let job = store
        .add_shell_job(
            None,
            Schedule::At {
                at: Utc::now() + chrono::Duration::seconds(1),
            },
            "once",
        )
        .await
        .unwrap();
    assert!(store.claim_slot(&job, job.next_run).await.unwrap());
    assert!(!store.get_job(&job.id).await.unwrap().enabled);
    assert!(!store.claim_slot(&job, job.next_run).await.unwrap());
    assert!(store.due_jobs(job.next_run).await.unwrap().is_empty());
}

#[tokio::test]
async fn claim_rejects_an_execution_definition_edited_after_due_selection() {
    let store = store();
    let job = store
        .add_shell_job(None, Schedule::Every { every_ms: 60000 }, "old command")
        .await
        .unwrap();
    let edited = store
        .update_job(
            &job.id,
            CronJobPatch {
                command: Some("new command".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(
        !store
            .claim_slot(&job, job.next_run + chrono::Duration::minutes(1))
            .await
            .unwrap()
    );
    assert_eq!(
        store.get_job(&job.id).await.unwrap().next_run,
        edited.next_run
    );
    assert!(
        store
            .claim_slot(&edited, edited.next_run + chrono::Duration::minutes(1))
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn completion_preserves_auto_delete_canceled_during_execution() {
    let store = store();
    let job = store
        .add_shell_job(
            None,
            Schedule::At {
                at: Utc::now() + chrono::Duration::hours(1),
            },
            "once",
        )
        .await
        .unwrap();
    let job = store
        .update_job(
            &job.id,
            CronJobPatch {
                delete_after_run: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(store.claim_slot(&job, job.next_run).await.unwrap());
    store
        .update_job(
            &job.id,
            CronJobPatch {
                delete_after_run: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    store
        .complete_run(
            &job,
            &Completion {
                remove: true,
                disable: false,
                next_run: None,
                finished_at: Utc::now(),
                success: true,
                output: "done".into(),
            },
        )
        .await
        .unwrap();
    let kept = store.get_job(&job.id).await.unwrap();
    assert!(!kept.enabled);
    assert!(!kept.delete_after_run);
    assert_eq!(kept.last_output.as_deref(), Some("done"));
}

#[tokio::test]
async fn completion_preserves_one_shot_reenabled_during_execution() {
    for (success, remove) in [(false, false), (true, false), (true, true)] {
        let store = store();
        let job = store
            .add_shell_job(
                None,
                Schedule::At {
                    at: Utc::now() + chrono::Duration::hours(1),
                },
                "once",
            )
            .await
            .unwrap();
        let job = store
            .update_job(
                &job.id,
                CronJobPatch {
                    delete_after_run: Some(remove),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(store.claim_slot(&job, job.next_run).await.unwrap());
        store
            .update_job(
                &job.id,
                CronJobPatch {
                    enabled: Some(true),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let completion = Completion {
            remove,
            disable: !remove,
            next_run: None,
            finished_at: Utc::now(),
            success,
            output: "done".into(),
        };
        store.complete_run(&job, &completion).await.unwrap();
        let kept = store.get_job(&job.id).await.unwrap();
        assert!(kept.enabled, "completion overwrote the explicit re-enable");
        assert_eq!(kept.last_output.as_deref(), Some("done"));
    }
}

#[tokio::test]
async fn due_selection_replaces_an_excluded_head_before_applying_the_batch_limit() {
    let store = store().with_limits(50, 1);
    let command = "first";
    let first = store
        .add_shell_job(None, Schedule::Every { every_ms: 60_000 }, command)
        .await
        .unwrap();
    let command = "second";
    let second = store
        .add_shell_job(None, Schedule::Every { every_ms: 60_000 }, command)
        .await
        .unwrap();
    let now = second.next_run + chrono::Duration::days(1);
    let due = store
        .due_jobs_excluding(now, std::slice::from_ref(&first.id))
        .await
        .unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].id, second.id);
}
