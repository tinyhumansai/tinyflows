use super::cooperative_stop_tests::Fixture;
use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::Notify;

#[tokio::test]
async fn scheduled_heartbeat_error_and_shutdown_wait_for_execute_then_release_claim() {
    let fixture = Arc::new(Fixture {
        now: "2026-01-01T00:00:00Z".parse().unwrap(),
        block_on_renewal: usize::MAX,
        renewals: AtomicUsize::new(0),
        renewal_entered: Notify::new(),
        release_renewal: Notify::new(),
        heartbeat_tick: Notify::new(),
        renewal_failed: Notify::new(),
        terminal_commit_entered: Notify::new(),
        release_terminal_commit: Notify::new(),
        persisted_completions: AtomicUsize::new(0),
        pause_terminal_commit: false,
        heartbeat_renew_error: false,
        execution_entered: Notify::new(),
        release_execution: Notify::new(),
        pause_execution: std::sync::atomic::AtomicBool::new(true),
        execution_in_flight: std::sync::atomic::AtomicBool::new(false),
        released_during_execution: std::sync::atomic::AtomicBool::new(false),
        claim_lost: std::sync::atomic::AtomicBool::new(false),
        fail_next_renewal: std::sync::atomic::AtomicBool::new(false),
        released_claims: AtomicUsize::new(0),
        executions: AtomicUsize::new(0),
        failed_callbacks: AtomicUsize::new(0),
        deliveries: AtomicUsize::new(0),
        fail_execution: true,
    });
    let ports = fixture.ports();
    let config = SchedulerConfig::default();
    let claim = fixture.claim();
    let (shutdown, shutdown_receiver) = watch::channel(false);
    let worker = tokio::spawn(async move {
        run_claim_with_renewal(
            &ports,
            &config,
            claim,
            ManualDelivery::Requested,
            false,
            None,
            shutdown_receiver,
        )
        .await
    });
    fixture.execution_entered.notified().await;
    fixture.fail_next_renewal.store(true, Ordering::SeqCst);
    fixture.heartbeat_tick.notify_one();
    fixture.renewal_failed.notified().await;
    shutdown.send_replace(true);
    fixture.claim_lost.store(true, Ordering::SeqCst);
    fixture.release_execution.notify_one();

    assert!(worker.await.unwrap().is_err());
    assert_eq!(fixture.executions.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.failed_callbacks.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.deliveries.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.persisted_completions.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.released_claims.load(Ordering::SeqCst), 1);
    assert!(!fixture.released_during_execution.load(Ordering::SeqCst));
}
