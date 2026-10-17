//! Module-owned reservations and retained lifecycle state for detached manual runs.
use super::super::{schedule_callbacks::HostCallbacks, schedule_store::Store};
use std::future::Future;
use std::{collections::HashMap, sync::Mutex, time::Duration};
use tinybus::Connection;
use tinyflows_bus::schedule::{
    ManualRunHandle, ManualRunOutcome, ManualRunRequest, ManualRunState, PrepareManualRun,
};
use tinyflows_schedule::runtime::{Clock, JobStore, RunRecord, Scheduler, SchedulerConfig};
use tokio::{
    sync::{Mutex as AsyncMutex, watch},
    time::Instant,
};

const PREPARED_TTL: Duration = Duration::from_secs(15 * 60);

pub(super) struct ManualWorkerEntry {
    control: Mutex<Control>,
    pub(super) stop: watch::Sender<bool>,
}

struct Control {
    state: ManualRunState,
    fingerprint: Option<serde_json::Value>,
    stop_requested: bool,
    expires_at: Instant,
    run_id: Option<String>,
}

impl ManualWorkerEntry {
    fn prepared() -> Self {
        let (stop, _) = watch::channel(false);
        Self {
            control: Mutex::new(Control {
                state: ManualRunState::Prepared,
                fingerprint: None,
                stop_requested: false,
                expires_at: Instant::now() + PREPARED_TTL,
                run_id: None,
            }),
            stop,
        }
    }

    pub(super) fn state(&self) -> ManualRunState {
        self.control
            .lock()
            .expect("manual worker control poisoned")
            .state
            .clone()
    }

    pub(super) fn begin_start(&self, fingerprint: serde_json::Value) -> anyhow::Result<bool> {
        let mut control = self.control.lock().expect("manual worker control poisoned");
        if let Some(existing) = &control.fingerprint {
            anyhow::ensure!(
                existing == &fingerprint,
                "manual worker start payload changed"
            );
            return Ok(false);
        }
        anyhow::ensure!(
            matches!(control.state, ManualRunState::Prepared),
            "manual worker handle is no longer startable"
        );
        control.fingerprint = Some(fingerprint);
        control.state = ManualRunState::Starting;
        Ok(true)
    }

    pub(super) fn stopped(&self) -> bool {
        self.control
            .lock()
            .expect("manual worker control poisoned")
            .stop_requested
    }

    pub(super) fn set_run_id(&self, run_id: String) {
        self.control
            .lock()
            .expect("manual worker control poisoned")
            .run_id = Some(run_id);
    }

    fn run_id(&self) -> Option<String> {
        self.control
            .lock()
            .expect("manual worker control poisoned")
            .run_id
            .clone()
    }

    pub(super) fn set_state(&self, state: ManualRunState) {
        let mut control = self.control.lock().expect("manual worker control poisoned");
        control.state = state;
    }

    pub(super) fn set_running(&self, run_id: String) {
        let mut control = self.control.lock().expect("manual worker control poisoned");
        control.state = if control.stop_requested {
            ManualRunState::Stopping {
                run_id: Some(run_id),
            }
        } else {
            ManualRunState::Running { run_id }
        };
    }

    pub(super) fn request_stop(&self) -> ManualRunState {
        let mut control = self.control.lock().expect("manual worker control poisoned");
        match control.state.clone() {
            ManualRunState::Prepared => {
                control.stop_requested = true;
                control.state = ManualRunState::Terminal {
                    outcome: ManualRunOutcome::Stopped { run_id: None },
                };
            }
            ManualRunState::Starting => {
                control.stop_requested = true;
                control.state = ManualRunState::Stopping { run_id: None };
                self.stop.send_replace(true);
            }
            ManualRunState::Running { run_id } => {
                control.stop_requested = true;
                control.state = ManualRunState::Stopping {
                    run_id: Some(run_id),
                };
                self.stop.send_replace(true);
            }
            ManualRunState::Stopping { .. } | ManualRunState::CleanupPending { .. } => {
                control.stop_requested = true;
                self.stop.send_replace(true);
            }
            ManualRunState::Terminal { .. } => {}
        }
        control.state.clone()
    }

    fn expired(&self, now: Instant) -> bool {
        let control = self.control.lock().expect("manual worker control poisoned");
        matches!(control.state, ManualRunState::Prepared) && control.expires_at <= now
    }
}

struct Reservation {
    scope: String,
    request_id: String,
    entry: std::sync::Arc<ManualWorkerEntry>,
}

#[derive(Default)]
pub(super) struct ManualWorkers {
    by_handle: HashMap<String, Reservation>,
    by_request: HashMap<(String, String), String>,
}

impl ManualWorkers {
    pub(super) fn prepare(&mut self, request: PrepareManualRun) -> ManualRunHandle {
        self.prune_expired();
        let key = (request.scope.clone(), request.request_id.clone());
        if let Some(handle) = self.by_request.get(&key) {
            return ManualRunHandle {
                token: handle.clone(),
            };
        }
        let handle = uuid::Uuid::new_v4().to_string();
        self.by_request.insert(key, handle.clone());
        self.by_handle.insert(
            handle.clone(),
            Reservation {
                scope: request.scope,
                request_id: request.request_id,
                entry: std::sync::Arc::new(ManualWorkerEntry::prepared()),
            },
        );
        ManualRunHandle { token: handle }
    }

    pub(super) fn get(&mut self, handle: &ManualRunHandle) -> Option<ReservationView> {
        self.prune_expired();
        self.by_handle
            .get(&handle.token)
            .map(|reservation| ReservationView {
                scope: reservation.scope.clone(),
                request_id: reservation.request_id.clone(),
                entry: reservation.entry.clone(),
            })
    }

    pub(super) fn acknowledge(&mut self, handle: &ManualRunHandle) -> bool {
        self.prune_expired();
        let Some(reservation) = self.by_handle.get(&handle.token) else {
            return false;
        };
        if !matches!(reservation.entry.state(), ManualRunState::Terminal { .. }) {
            return false;
        }
        let reservation = self.by_handle.remove(&handle.token).unwrap();
        self.by_request
            .remove(&(reservation.scope, reservation.request_id));
        true
    }

    pub(super) fn request_stop_all(&mut self) {
        for reservation in self.by_handle.values() {
            reservation.entry.request_stop();
        }
    }

    fn prune_expired(&mut self) {
        let now = Instant::now();
        let expired: Vec<_> = self
            .by_handle
            .iter()
            .filter(|&(_, reservation)| reservation.entry.expired(now))
            .map(|(handle, _)| handle.clone())
            .collect();
        for handle in expired {
            if let Some(reservation) = self.by_handle.remove(&handle) {
                self.by_request
                    .remove(&(reservation.scope, reservation.request_id));
            }
        }
    }
}

pub(super) struct ReservationView {
    pub(super) scope: String,
    pub(super) request_id: String,
    pub(super) entry: std::sync::Arc<ManualWorkerEntry>,
}

pub(super) async fn run_manual_worker(
    connection: Connection,
    scope: String,
    mut request: ManualRunRequest,
    entry: std::sync::Arc<ManualWorkerEntry>,
) {
    let run_id = if request.run_id.trim().is_empty() {
        uuid::Uuid::new_v4().to_string()
    } else {
        request.run_id.clone()
    };
    request.run_id = run_id.clone();
    entry.set_run_id(run_id);
    let claim = std::sync::Arc::new(AsyncMutex::new(None));
    let worker_claim = claim.clone();
    let worker_entry = entry.clone();
    let worker = async move {
        run_manual_worker_body(connection, scope, request, worker_entry, worker_claim).await
    };
    supervise_manual_worker(entry, claim, worker).await;
}

async fn supervise_manual_worker<F>(
    entry: std::sync::Arc<ManualWorkerEntry>,
    claim: std::sync::Arc<AsyncMutex<Option<HeldClaim>>>,
    worker: F,
) where
    F: Future<Output = anyhow::Result<ManualRunOutcome>> + Send + 'static,
{
    let worker = tokio::spawn(worker);
    let result = worker.await;
    let run_id = entry.run_id();
    let outcome = match result {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(error)) => ManualRunOutcome::Failed {
            run_id: run_id.clone(),
            message: error.to_string(),
        },
        Err(error) => ManualRunOutcome::Failed {
            run_id,
            message: format!("manual worker task failed: {error}"),
        },
    };
    if let Some(claim) = claim.lock().await.take() {
        release_until_safe(&entry, claim).await;
    }
    entry.set_state(ManualRunState::Terminal { outcome });
}

async fn run_manual_worker_body(
    connection: Connection,
    scope: String,
    request: ManualRunRequest,
    entry: std::sync::Arc<ManualWorkerEntry>,
    claim_slot: std::sync::Arc<AsyncMutex<Option<HeldClaim>>>,
) -> anyhow::Result<ManualRunOutcome> {
    if entry.stopped() {
        return Ok(ManualRunOutcome::Stopped { run_id: None });
    }
    let config: SchedulerConfig = serde_json::from_value(request.config)?;
    let mut ports = HostCallbacks::ports(connection.clone(), request.callback);
    ports.store = std::sync::Arc::new(Store::new(connection, request.storage).await?);
    ports.store.validate_manual_claim_support().await?;
    if entry.stopped() {
        return Ok(ManualRunOutcome::Stopped { run_id: None });
    }
    let run_id = request.run_id;
    let owner = uuid::Uuid::new_v4().to_string();
    *claim_slot.lock().await = Some(HeldClaim::Claiming {
        job: request.job.clone(),
        owner: owner.clone(),
        run_id: Some(run_id.clone()),
        store: ports.store.clone(),
        clock: ports.clock.clone(),
    });
    let mut claim = match ports
        .store
        .claim_manual_occurrence(
            &request.job,
            &owner,
            ports.clock.now(),
            chrono::Duration::hours(2),
        )
        .await?
    {
        Ok(claim) => claim,
        Err(reason) => {
            claim_slot.lock().await.take();
            return Ok(ManualRunOutcome::Refused { run_id, reason });
        }
    };
    claim.run_id = Some(run_id.clone());
    *claim_slot.lock().await = Some(HeldClaim::Claimed {
        claim: claim.clone(),
        store: ports.store.clone(),
        clock: ports.clock.clone(),
    });
    if entry.stopped() {
        return Ok(ManualRunOutcome::Stopped {
            run_id: Some(run_id),
        });
    }
    let queued_at = ports.clock.now();
    let queued = RunRecord {
        run_id: run_id.clone(),
        job_id: claim.job.id.clone(),
        job_created_at: claim.job.created_at,
        started_at: claim.started_at,
        finished_at: queued_at,
        status: "queued".into(),
        output: String::new(),
        delivery_status: None,
    };
    if !ports.store.queue_occurrence(&claim, queued).await? {
        return Ok(ManualRunOutcome::Refused {
            run_id,
            reason: tinyflows_bus::schedule::RunJobRefusal::DefinitionChanged,
        });
    }
    entry.set_running(run_id.clone());
    if entry.stopped() {
        return Ok(ManualRunOutcome::Stopped {
            run_id: Some(run_id),
        });
    }
    let scheduler = Scheduler::new(scope, config, ports);
    match scheduler
        .run_manual_claim_cooperative(claim, request.delivery, entry.stop.subscribe())
        .await?
    {
        Some(outcome) => {
            claim_slot.lock().await.take();
            Ok(ManualRunOutcome::Completed {
                run_id,
                attempt: outcome.attempt,
                success: outcome.success,
                delivery_status: outcome.delivery_status,
            })
        }
        None => Ok(ManualRunOutcome::Stopped {
            run_id: Some(run_id),
        }),
    }
}

async fn release_until_safe(entry: &ManualWorkerEntry, mut held: HeldClaim) {
    let mut delay = Duration::from_millis(250);
    loop {
        match held {
            HeldClaim::Claiming {
                job,
                owner,
                run_id,
                store,
                clock,
            } => match store
                .find_owned_manual_claim(&job, &owner, clock.now())
                .await
            {
                Ok(Some(mut claim)) => {
                    claim.run_id = run_id;
                    held = HeldClaim::Claimed {
                        claim,
                        store,
                        clock,
                    };
                }
                Ok(None) => {
                    return;
                }
                Err(error) => {
                    entry.set_state(ManualRunState::CleanupPending {
                        run_id: run_id.clone(),
                    });
                    tracing::warn!(%error, "manual claim admission reconciliation will retry");
                    clock.sleep(delay).await;
                    delay = delay.saturating_mul(2).min(Duration::from_secs(30));
                    held = HeldClaim::Claiming {
                        job,
                        owner,
                        run_id,
                        store,
                        clock,
                    };
                }
            },
            HeldClaim::Claimed {
                claim,
                store,
                clock,
            } => match store.release_occurrence(&claim, clock.now()).await {
                Ok(_) => return,
                Err(error) => {
                    entry.set_state(ManualRunState::CleanupPending {
                        run_id: claim.run_id.clone(),
                    });
                    tracing::warn!(%error, run_id = ?claim.run_id, "manual worker claim release will retry");
                    clock.sleep(delay).await;
                    delay = delay.saturating_mul(2).min(Duration::from_secs(30));
                    held = HeldClaim::Claimed {
                        claim,
                        store,
                        clock,
                    };
                }
            },
        }
    }
}

enum HeldClaim {
    Claiming {
        job: tinyflows_bus::schedule::CronJob,
        owner: String,
        run_id: Option<String>,
        store: std::sync::Arc<dyn JobStore>,
        clock: std::sync::Arc<dyn Clock>,
    },
    Claimed {
        claim: tinyflows_bus::schedule::OccurrenceClaim,
        store: std::sync::Arc<dyn JobStore>,
        clock: std::sync::Arc<dyn Clock>,
    },
}

#[cfg(test)]
#[path = "schedule_manual_workers_tests.rs"]
mod tests;
