//! Parked-run expiry and local startup recovery.

use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::Value;
use tinyflows::engine::Checkpointer as _;
use tinyflows_bus::runs::{
    BootSweep, BootSweepResponse, ExpireParked, ExpireParkedResponse, RunEvent,
};
use tinyflows_drivers::{DriverCheckpointer, catalog::FlowCatalogDocuments};
use tinystoragedrivers_core::DocumentStore;

use super::preparation::catalog;
use super::{PARKED, active, active_lease, key, unregister};
use crate::catalog_documents::Documents;

const BOOT_REASON: &str = "Run interrupted by module restart; no local actor owns this run.";

pub(super) async fn expire_parked(
    connection: &tinybus::Connection,
    request: ExpireParked,
) -> Result<ExpireParkedResponse> {
    let catalog = catalog(connection, request.storage.clone()).await?;
    let now = chrono::Utc::now();
    let cutoff = (now
        - chrono::Duration::seconds(tinyflows_catalog::run_management::PARKED_RUN_TTL_SECS))
    .to_rfc3339();
    let candidates = catalog.list_expirable_parked_run_ids(&cutoff).await?;
    let now = now.to_rfc3339();
    let expiry_message = tinyflows_catalog::run_management::parked_expiry_message(
        tinyflows_catalog::run_management::PARKED_RUN_TTL_SECS,
    );
    let mut expired = Vec::new();
    let mut terminal_error = None;
    for (run_id, flow_id) in candidates {
        let changed = match catalog
            .expire_parked_run(&run_id, &cutoff, &now, &expiry_message)
            .await
        {
            Ok(changed) => changed,
            Err(error) => {
                terminal_error.get_or_insert_with(|| {
                    anyhow::anyhow!("expiring parked run {run_id}: {error:#}")
                });
                continue;
            }
        };
        if !changed {
            continue;
        }
        expired.push((run_id.clone(), flow_id));
        let snapshot = match catalog.flow_run_snapshot(&run_id).await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                terminal_error.get_or_insert_with(|| {
                    anyhow::anyhow!("reading expired run {run_id} snapshot: {error:#}")
                });
                None
            }
        };
        let live_lease = snapshot
            .as_ref()
            .and_then(|value| value.get("lease"))
            .and_then(Value::as_str)
            .and_then(active_lease);
        let parked_actor = live_lease.or_else(|| active(&key(&request.storage, &run_id)));
        if let Some(actor) = parked_actor
            && actor.phase.load(std::sync::atomic::Ordering::Acquire) == PARKED
        {
            unregister(&actor);
        }
        settle_snapshot_dedup(&catalog, snapshot.as_ref(), &run_id).await;
        let run = match catalog.get_flow_run(&run_id).await {
            Ok(run) => run,
            Err(error) => {
                terminal_error.get_or_insert_with(|| {
                    anyhow::anyhow!("reading expired run {run_id}: {error:#}")
                });
                None
            }
        };
        if let Some(run) = run {
            match Documents::new(connection.clone(), request.storage.callback.clone()).await {
                Ok(documents) => {
                    let checkpointer = DriverCheckpointer::<Value>::new(Arc::new(documents));
                    if let Err(error) = checkpointer.delete_thread(&run.thread_id).await {
                        tracing::warn!(%error, run_id, "expired run checkpoint cleanup failed");
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, run_id, "opening expired run checkpoint scope failed")
                }
            }
        }
        if let Err(error) =
            settle_terminal_event(&catalog, &run_id, "cancelled", "parked run expired").await
        {
            terminal_error
                .get_or_insert_with(|| anyhow::anyhow!("settling expired run {run_id}: {error:#}"));
        }
    }
    if let Some(error) = terminal_error {
        return Err(error);
    }
    Ok(ExpireParkedResponse { expired })
}

pub(super) async fn boot_sweep(
    connection: &tinybus::Connection,
    request: BootSweep,
) -> Result<BootSweepResponse> {
    if tinyflows_catalog::run_management::boot_sweep_plan(request.shared_backend)
        == tinyflows_catalog::run_management::BootSweepPlan::Nothing
    {
        return Ok(BootSweepResponse {
            interrupted: vec![],
        });
    }
    let catalog = catalog(connection, request.storage.clone()).await?;
    let floor = chrono::Utc::now().to_rfc3339();
    let candidates = catalog.list_running_run_ids(&floor).await?;
    let checkpointer_docs: Arc<dyn DocumentStore> = Arc::new(
        Documents::new(connection.clone(), request.storage.callback.clone())
            .await
            .context("opening startup checkpoint scope")?,
    );
    let checkpointer = DriverCheckpointer::<Value>::new(checkpointer_docs);
    let mut interrupted = Vec::new();
    let mut terminal_error = None;
    for (run_id, _) in candidates {
        if active(&key(&request.storage, &run_id)).is_some() {
            continue;
        }
        let snapshot = match catalog.flow_run_snapshot(&run_id).await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                terminal_error.get_or_insert_with(|| {
                    anyhow::anyhow!("reading startup run {run_id} snapshot: {error:#}")
                });
                continue;
            }
        };
        if snapshot
            .as_ref()
            .and_then(|value| value.get("lease"))
            .and_then(Value::as_str)
            .and_then(active_lease)
            .is_some()
        {
            continue;
        }
        let run = match catalog.get_flow_run(&run_id).await {
            Ok(Some(run)) => run,
            Ok(None) => continue,
            Err(error) => {
                terminal_error.get_or_insert_with(|| {
                    anyhow::anyhow!("reading startup run {run_id}: {error:#}")
                });
                continue;
            }
        };
        if !catalog
            .mark_run_interrupted(&run_id, &floor, BOOT_REASON)
            .await?
        {
            continue;
        }
        settle_snapshot_dedup(&catalog, snapshot.as_ref(), &run_id).await;
        if let Err(error) = checkpointer.delete_thread(&run.thread_id).await {
            tracing::warn!(%error, run_id, "orphaned run checkpoint cleanup failed");
        }
        if let Err(error) =
            settle_terminal_event(&catalog, &run_id, "interrupted", BOOT_REASON).await
        {
            terminal_error.get_or_insert_with(|| {
                anyhow::anyhow!("settling interrupted run {run_id}: {error:#}")
            });
        }
        interrupted.push(run_id);
    }
    if let Some(error) = terminal_error {
        return Err(error);
    }
    Ok(BootSweepResponse { interrupted })
}

async fn settle_terminal_event(
    catalog: &FlowCatalogDocuments,
    run_id: &str,
    status: &str,
    error: &str,
) -> Result<()> {
    let seq = catalog
        .flow_run_event_last_seq(run_id)
        .await?
        .saturating_add(1);
    catalog
        .append_flow_run_event(
            run_id,
            serde_json::to_value(RunEvent {
                seq,
                kind: "finished".into(),
                data: serde_json::to_value(super::events::finished_payload(
                    status,
                    Some(error.to_owned()),
                    None,
                ))?,
                graph_run_id: None,
            })?,
        )
        .await?;
    if !catalog.mark_flow_run_terminal_event(run_id, seq).await? {
        anyhow::bail!("terminal event cursor was not committed")
    }
    Ok(())
}

pub(super) async fn settle_snapshot_dedup(
    catalog: &FlowCatalogDocuments,
    snapshot: Option<&Value>,
    run_id: &str,
) {
    let Some(snapshot) = snapshot else {
        return;
    };
    let decoded = (|| {
        Some((
            tinyflows::migrate::deserialize_graph(snapshot.get("graph")?.clone()).ok()?,
            serde_json::from_value::<tinyflows_bus::runs::RunContext>(
                snapshot.get("context")?.clone(),
            )
            .ok()?,
        ))
    })();
    if let Some((graph, context)) = decoded {
        super::actor::settle_dedup(catalog.clone(), &context, &graph, false, run_id).await;
    } else {
        tracing::warn!(
            run_id,
            "run snapshot could not be decoded for dedup cleanup"
        );
    }
}
