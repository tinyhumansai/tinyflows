//! CAS-guarded parked and stale run cancellation.

use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::Value;
use tinybus::Connection;
use tinyflows::graph::checkpoint::Checkpointer as _;
use tinyflows_bus::runs::{CancelRunResponse, RunControlRequest, RunEvent};
use tinyflows_drivers::{DriverCheckpointer, catalog::FlowCatalogDocuments};
use tinystoragedrivers_core::DocumentStore;

use super::events::EventWriter;
use super::preparation::Prepared;
use super::{ActiveRun, RUNNING, unregister};
use crate::catalog_documents::Documents;

pub(super) async fn cancel_parked_or_stale(
    connection: &Connection,
    request: RunControlRequest,
    active: Option<Arc<ActiveRun>>,
) -> Result<CancelRunResponse> {
    if let Some(active) = &active
        && active.phase.load(std::sync::atomic::Ordering::Acquire) == RUNNING
    {
        active.token.cancel();
        return Ok(CancelRunResponse { signalled: true });
    }
    let docs =
        Arc::new(Documents::new(connection.clone(), request.storage.callback.clone()).await?);
    let document_port = Arc::clone(&docs) as Arc<dyn DocumentStore>;
    let catalog = FlowCatalogDocuments::new(Arc::clone(&document_port));
    let Some(run) = catalog.get_flow_run(&request.run_id).await? else {
        anyhow::bail!("flow run does not exist in this owner scope")
    };
    if let Some(lease) = catalog
        .flow_run_snapshot(&request.run_id)
        .await?
        .and_then(|snapshot| {
            snapshot
                .get("lease")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        && let Some(live) = super::active_lease(&lease)
        && !active
            .as_ref()
            .is_some_and(|candidate| Arc::ptr_eq(candidate, &live))
    {
        return Ok(CancelRunResponse { signalled: false });
    }
    if !matches!(run.status.as_str(), "running" | "pending_approval") {
        if let Some(active) = active {
            unregister(&active);
        }
        return Ok(CancelRunResponse { signalled: false });
    }
    let checkpointer = DriverCheckpointer::<Value>::new(document_port);
    let parked = run.status == "pending_approval";
    let error = if parked {
        "parked run cancelled"
    } else {
        "orphaned running run cancelled"
    };
    let now = chrono::Utc::now().to_rfc3339();
    let changed = if parked {
        catalog
            .finish_parked_flow_run(
                &request.run_id,
                "cancelled",
                &now,
                &run.steps,
                &[],
                Some(error),
                None,
            )
            .await?
    } else {
        catalog
            .finish_flow_run(
                &request.run_id,
                "cancelled",
                &now,
                &run.steps,
                &[],
                Some(error),
                None,
            )
            .await?
    };
    if !changed {
        return Ok(CancelRunResponse { signalled: false });
    }
    if let Err(error) =
        cancel_snapshot(&catalog, &checkpointer, &request.run_id, &run.thread_id).await
    {
        tracing::warn!(%error, run_id = %request.run_id, "parked/stale cancellation cleanup failed");
    }
    append_cancelled_event(&catalog, &request.run_id, error).await?;
    if let Some(active) = active {
        unregister(&active);
    }
    Ok(CancelRunResponse { signalled: true })
}

pub(super) async fn cancel_parked(
    prepared: &Prepared,
    run_id: &str,
    writer: &EventWriter,
) -> Result<bool> {
    let Some(run) = prepared.catalog.get_flow_run(run_id).await? else {
        return Ok(false);
    };
    if run.status != "pending_approval" {
        return Ok(false);
    }
    let now = chrono::Utc::now().to_rfc3339();
    let changed = prepared
        .catalog
        .finish_parked_flow_run(
            run_id,
            "cancelled",
            &now,
            &run.steps,
            &[],
            Some("parked run cancelled"),
            None,
        )
        .await?;
    if !changed {
        return Ok(false);
    }
    if let Err(error) = cancel_snapshot(
        &prepared.catalog,
        &prepared.checkpointer,
        run_id,
        &run.thread_id,
    )
    .await
    {
        tracing::warn!(%error, run_id, "parked cancellation cleanup failed");
    }
    let seq = writer
        .emit_terminal(super::events::finished_payload(
            "cancelled",
            Some("parked run cancelled".into()),
            None,
        ))
        .await?;
    if !prepared
        .catalog
        .mark_flow_run_terminal_event(run_id, seq)
        .await?
    {
        anyhow::bail!("parked cancellation terminal cursor was not committed")
    }
    Ok(true)
}

async fn cancel_snapshot(
    catalog: &FlowCatalogDocuments,
    checkpointer: &DriverCheckpointer<Value>,
    run_id: &str,
    thread_id: &str,
) -> Result<()> {
    if let Some(snapshot) = catalog.flow_run_snapshot(run_id).await? {
        let graph = snapshot
            .get("graph")
            .cloned()
            .context("run snapshot omitted graph")?;
        let graph = tinyflows::migrate::deserialize_graph(graph)
            .map_err(anyhow::Error::msg)
            .context("decoding executed graph snapshot")?;
        let context: tinyflows_bus::runs::RunContext = serde_json::from_value(
            snapshot
                .get("context")
                .cloned()
                .context("run snapshot omitted context")?,
        )
        .context("decoding run context snapshot")?;
        super::actor::settle_dedup(catalog.clone(), &context, &graph, false, run_id).await;
    }
    checkpointer.delete_thread(thread_id).await?;
    Ok(())
}

async fn append_cancelled_event(
    catalog: &FlowCatalogDocuments,
    run_id: &str,
    reason: &str,
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
                    "cancelled",
                    Some(reason.to_owned()),
                    None,
                ))?,
                graph_run_id: None,
            })?,
        )
        .await?;
    if !catalog.mark_flow_run_terminal_event(run_id, seq).await? {
        anyhow::bail!("cancellation terminal cursor was not committed")
    }
    Ok(())
}
