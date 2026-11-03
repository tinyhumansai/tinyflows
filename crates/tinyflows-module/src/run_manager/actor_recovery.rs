//! Lease-fenced cleanup for a run actor future that is dropped or panics.

use std::sync::Arc;

use serde_json::Value;
use tinyflows_bus::catalog::CatalogStorage;
use tinyflows_bus::runs::RunEvent;
use tinyflows_drivers::{DriverCheckpointer, catalog::FlowCatalogDocuments};

use super::actor::delete_checkpoints;
use super::dedup_settlement::from_snapshot;
use super::events::finished_payload;
use super::{ActiveRun, TERMINAL, unregister};

const STOPPED_ACTOR_REASON: &str =
    "Run interrupted because its module actor stopped before settlement.";

/// Captures recovery ownership before the actor future is returned to Tokio.
/// The lease check in the driver's terminal CAS keeps an old guard from
/// interrupting a replacement resume or a legitimate parked/terminal row.
pub(super) struct ActorRecoveryGuard {
    catalog: FlowCatalogDocuments,
    checkpointer: DriverCheckpointer<Value>,
    storage: CatalogStorage,
    active: Arc<ActiveRun>,
    run_id: String,
    thread_id: String,
    lease: String,
    armed: bool,
}

impl ActorRecoveryGuard {
    pub(super) fn new(
        catalog: &FlowCatalogDocuments,
        checkpointer: &DriverCheckpointer<Value>,
        storage: &CatalogStorage,
        run_id: &str,
        thread_id: &str,
        active: Arc<ActiveRun>,
    ) -> Self {
        Self {
            catalog: catalog.clone(),
            checkpointer: checkpointer.clone(),
            storage: storage.clone(),
            run_id: run_id.to_owned(),
            thread_id: thread_id.to_owned(),
            lease: active.lease.clone(),
            active,
            armed: true,
        }
    }

    pub(super) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ActorRecoveryGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::warn!(
                target: "workflows",
                run_id = %self.run_id,
                "actor stopped without a Tokio runtime; boot recovery will reconcile the run"
            );
            unregister(&self.active);
            return;
        };

        let recovery = ActorRecovery {
            catalog: self.catalog.clone(),
            checkpointer: self.checkpointer.clone(),
            storage: self.storage.clone(),
            active: Arc::clone(&self.active),
            run_id: self.run_id.clone(),
            thread_id: self.thread_id.clone(),
            lease: self.lease.clone(),
        };
        // Capture the identity-aware registry guard in the spawned future's
        // environment, so even cancellation before its first poll releases
        // this entry without removing a replacement actor.
        let active_cleanup = ActiveCleanupGuard(Arc::clone(&self.active));
        #[cfg(test)]
        let run_id = self.run_id.clone();
        runtime.spawn(async move {
            let _active_cleanup = active_cleanup;
            recovery.reconcile().await;
            drop(_active_cleanup);
            #[cfg(test)]
            super::actor_recovery_tests::notify_recovery_finished_for_test(&run_id);
        });
    }
}

struct ActiveCleanupGuard(Arc<ActiveRun>);

impl Drop for ActiveCleanupGuard {
    fn drop(&mut self) {
        unregister(&self.0);
    }
}

struct ActorRecovery {
    catalog: FlowCatalogDocuments,
    checkpointer: DriverCheckpointer<Value>,
    storage: CatalogStorage,
    active: Arc<ActiveRun>,
    run_id: String,
    thread_id: String,
    lease: String,
}

impl ActorRecovery {
    async fn reconcile(self) {
        let snapshot = match self.catalog.flow_run_snapshot(&self.run_id).await {
            Ok(Some(snapshot))
                if snapshot.get("lease").and_then(Value::as_str) == Some(&self.lease) =>
            {
                snapshot
            }
            Ok(_) => return,
            Err(error) => {
                tracing::warn!(
                    target: "workflows",
                    run_id = %self.run_id,
                    %error,
                    "actor recovery could not read its execution snapshot"
                );
                return;
            }
        };
        let now = chrono::Utc::now().to_rfc3339();
        let interrupted = match self
            .catalog
            .interrupt_running_flow_run_for_lease(
                &self.run_id,
                &self.lease,
                &now,
                STOPPED_ACTOR_REASON,
            )
            .await
        {
            Ok(interrupted) => interrupted,
            Err(error) => {
                tracing::warn!(
                    target: "workflows",
                    run_id = %self.run_id,
                    %error,
                    "actor recovery could not settle its leased run"
                );
                return;
            }
        };
        if !interrupted {
            return;
        }
        self.active
            .phase
            .store(TERMINAL, std::sync::atomic::Ordering::Release);
        super::record_settled_summary(&self.catalog, &self.run_id, "interrupted").await;

        let _dedup_settlement = from_snapshot(
            &self.storage,
            self.catalog.clone(),
            Some(&snapshot),
            &self.run_id,
            false,
        )
        .await;
        delete_checkpoints(&self.checkpointer, &self.thread_id, &self.run_id).await;
        self.append_interrupted_event().await;
        drop(_dedup_settlement);
    }

    async fn append_interrupted_event(&self) {
        let seq = match self.catalog.flow_run_event_last_seq(&self.run_id).await {
            Ok(seq) => seq.saturating_add(1),
            Err(error) => {
                tracing::warn!(
                    target: "workflows",
                    run_id = %self.run_id,
                    %error,
                    "actor recovery could not read the terminal event cursor"
                );
                return;
            }
        };
        let event = RunEvent {
            seq,
            kind: "finished".into(),
            data: match serde_json::to_value(finished_payload(
                "interrupted",
                Some(STOPPED_ACTOR_REASON.to_owned()),
                None,
                None,
                false,
            )) {
                Ok(event) => event,
                Err(error) => {
                    tracing::error!(
                        target: "workflows",
                        run_id = %self.run_id,
                        %error,
                        "actor recovery could not encode its terminal event"
                    );
                    return;
                }
            },
            graph_run_id: None,
        };
        let event = match serde_json::to_value(event) {
            Ok(event) => event,
            Err(error) => {
                tracing::error!(
                    target: "workflows",
                    run_id = %self.run_id,
                    %error,
                    "actor recovery could not serialize its terminal event"
                );
                return;
            }
        };
        match self
            .catalog
            .append_flow_run_event(&self.run_id, event)
            .await
        {
            Ok(()) => match self
                .catalog
                .mark_flow_run_terminal_event(&self.run_id, seq)
                .await
            {
                Ok(true) => {}
                Ok(false) => tracing::error!(
                    target: "workflows",
                    run_id = %self.run_id,
                    "actor recovery terminal event cursor was not committed"
                ),
                Err(error) => tracing::error!(
                    target: "workflows",
                    run_id = %self.run_id,
                    %error,
                    "actor recovery could not commit its terminal event cursor"
                ),
            },
            Err(error) => tracing::error!(
                target: "workflows",
                run_id = %self.run_id,
                %error,
                "actor recovery could not persist its terminal event"
            ),
        }
    }
}
