//! Admission of an owner-scoped parked run for checkpoint continuation.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};
use tinyflows::compiler;
use tinyflows::engine::Checkpointer as _;
use tinyflows_bus::runs::{FlowRunTrigger, ResumeRun, ResumeRunResponse, RunContext, StartRun};
use tinyflows_drivers::{DriverCheckpointer, catalog::FlowCatalogDocuments};
use tinystoragedrivers_core::DocumentStore;

use super::preparation::Prepared;
use super::{PARKED, RUNNING, active, key, reserve_resume, unregister};
use crate::catalog_documents::Documents;

const GRAPH_CHANGED: &str = "workflow graph changed after the run parked; refusing resume";

pub(super) async fn resume(
    connection: &tinybus::Connection,
    request: ResumeRun,
) -> Result<ResumeRunResponse> {
    validate(&request)?;
    let catalog = super::preparation::catalog(connection, request.storage.clone()).await?;
    super::verify_live_owner(&catalog, &request.storage, &request.run_id).await?;
    let row = catalog
        .get_flow_run(&request.run_id)
        .await?
        .context("flow run does not exist in this owner scope")?;
    if row.flow_id != request.flow_id {
        bail!("flow run belongs to a different workflow")
    }
    if row.status != "pending_approval" {
        bail!(
            "flow run is not parked for approval (status: {})",
            row.status
        )
    }
    if let Some(gate) = request
        .approvals
        .iter()
        .find(|gate| request.rejections.contains(gate))
    {
        bail!("gate '{gate}' cannot be both approved and rejected")
    }
    if !request
        .approvals
        .iter()
        .chain(&request.rejections)
        .any(|gate| row.pending_approvals.contains(gate))
    {
        bail!("resume decisions do not name a currently pending gate")
    }

    let snapshot = catalog
        .flow_run_snapshot(&request.run_id)
        .await?
        .context("parked run has no execution snapshot")?;
    let context: RunContext = serde_json::from_value(
        snapshot
            .get("context")
            .cloned()
            .context("run snapshot omitted context")?,
    )
    .context("decoding run context snapshot")?;
    if context.flow_id != request.flow_id {
        bail!("run snapshot belongs to a different workflow")
    }
    let flow = catalog
        .get_flow(&request.flow_id)
        .await?
        .context("saved workflow does not exist in this owner scope")?;
    if let Some(expected) = row.graph_hash.as_deref() {
        let current =
            tinyflows_catalog::graph_hash::compute_graph_hash(&flow.graph, flow.require_approval);
        if current.as_deref() != Some(expected) {
            refuse_stale_graph(connection, &request, &catalog, &request.run_id, &row.steps).await?;
            bail!(GRAPH_CHANGED)
        }
    }

    let graph_json = snapshot
        .get("graph")
        .cloned()
        .context("run snapshot omitted graph")?;
    let graph = tinyflows::migrate::deserialize_graph(graph_json)
        .map_err(anyhow::Error::msg)
        .context("decoding pinned execution graph")?;
    tinyflows::compat::ensure_compatible(&graph).map_err(anyhow::Error::msg)?;
    let compiled = compiler::compile(&graph).context("compiling pinned execution graph")?;
    let inputs: Map<String, Value> = serde_json::from_value(
        snapshot
            .get("inputs")
            .cloned()
            .context("run snapshot omitted inputs")?,
    )
    .context("decoding run input snapshot")?;
    tinyflows::model::resolve_inputs(&compiled.graph.inputs, &inputs)
        .context("resolving pinned workflow inputs")?;
    let trigger_kind: FlowRunTrigger = serde_json::from_value(
        snapshot
            .get("trigger_kind")
            .cloned()
            .context("run snapshot omitted trigger kind")?,
    )
    .context("decoding run trigger kind")?;
    let trigger_data = snapshot
        .get("trigger_data")
        .cloned()
        .context("run snapshot omitted trigger data")?;
    let require_approval = snapshot
        .get("require_approval")
        .and_then(Value::as_bool)
        .context("run snapshot omitted approval policy")?;
    let run = StartRun {
        run_id: request.run_id.clone(),
        context,
        caps_host: request.caps_host.clone(),
        storage: request.storage.clone(),
        graph: serde_json::to_value(&compiled.graph)?,
        trigger_kind,
        trigger_data,
        inputs: inputs.clone(),
        timeout_ms: request.timeout_ms,
    };

    let documents: Arc<dyn DocumentStore> =
        Arc::new(Documents::new(connection.clone(), request.storage.callback.clone()).await?);
    let prepared = Prepared {
        catalog: catalog.clone(),
        compiled,
        inputs,
        checkpointer: DriverCheckpointer::new(documents),
        require_approval,
    };
    let run_key = key(&request.storage, &request.run_id);
    let active = reserve_resume(run_key)?;
    let claimed = match catalog
        .claim_parked_flow_run_for_resume(&request.run_id, &active.lease)
        .await
    {
        Ok(claimed) => claimed,
        Err(error) => {
            unregister(&active);
            return Err(error).context("claiming parked run for resume");
        }
    };
    if !claimed {
        unregister(&active);
        bail!("parked run was claimed by another lifecycle operation")
    }
    active
        .phase
        .store(RUNNING, std::sync::atomic::Ordering::Release);
    let (actor_ready, actor_admitted) = tokio::sync::oneshot::channel();
    tokio::spawn(super::actor::drive_resume(
        connection.clone(),
        run,
        prepared,
        Arc::clone(&active),
        actor_admitted,
        request,
        row.steps,
    ));
    active.admitted.notify_waiters();
    let _ = actor_ready.send(());
    Ok(ResumeRunResponse {
        run_id: active.key.run_id.clone(),
    })
}

fn validate(request: &ResumeRun) -> Result<()> {
    if request.run_id.trim().is_empty() || request.flow_id.trim().is_empty() {
        bail!("run id and flow id must not be empty")
    }
    if request.timeout_ms == 0 {
        bail!("run timeout_ms must be greater than zero")
    }
    for (name, destination, object_path) in [
        (
            "CatalogStorage",
            request.storage.callback.destination.as_str(),
            request.storage.callback.object_path.as_str(),
        ),
        (
            "CapsHost",
            request.caps_host.callback.destination.as_str(),
            request.caps_host.callback.object_path.as_str(),
        ),
    ] {
        if destination.trim().is_empty() || object_path.trim().is_empty() {
            bail!("{name} callback address must not be empty")
        }
    }
    Ok(())
}

async fn refuse_stale_graph(
    connection: &tinybus::Connection,
    request: &ResumeRun,
    catalog: &FlowCatalogDocuments,
    run_id: &str,
    steps: &[tinyflows_catalog::FlowRunStep],
) -> Result<()> {
    let snapshot = match catalog.flow_run_snapshot(run_id).await {
        Ok(snapshot) => snapshot,
        Err(error) => {
            tracing::warn!(%error, run_id, "stale-graph refusal could not read execution snapshot");
            None
        }
    };
    if !catalog
        .finish_parked_flow_run(
            run_id,
            "cancelled",
            &chrono::Utc::now().to_rfc3339(),
            steps,
            &[],
            Some(GRAPH_CHANGED),
            None,
        )
        .await?
    {
        return Ok(());
    }
    if let Some(active) = active(&key(&request.storage, run_id))
        && active.phase.load(std::sync::atomic::Ordering::Acquire) == PARKED
    {
        unregister(&active);
    }
    super::sweeps::settle_snapshot_dedup(catalog, snapshot.as_ref(), run_id).await;
    match Documents::new(connection.clone(), request.storage.callback.clone()).await {
        Ok(documents) => {
            let checkpointer = DriverCheckpointer::<Value>::new(Arc::new(documents));
            if let Err(error) = checkpointer.delete_thread(run_id).await {
                tracing::warn!(%error, run_id, "stale-graph refusal checkpoint cleanup failed");
            }
        }
        Err(error) => tracing::warn!(%error, run_id, "opening stale-run checkpoint scope failed"),
    }
    let seq = catalog
        .flow_run_event_last_seq(run_id)
        .await?
        .saturating_add(1);
    catalog
        .append_flow_run_event(
            run_id,
            serde_json::to_value(tinyflows_bus::runs::RunEvent {
                seq,
                kind: "finished".into(),
                data: serde_json::to_value(super::events::finished_payload(
                    "cancelled",
                    Some(GRAPH_CHANGED.to_owned()),
                    None,
                ))?,
                graph_run_id: None,
            })?,
        )
        .await?;
    if !catalog.mark_flow_run_terminal_event(run_id, seq).await? {
        bail!("stale-graph refusal terminal event cursor was not committed")
    }
    Ok(())
}
