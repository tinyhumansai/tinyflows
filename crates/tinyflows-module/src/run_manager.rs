//! Owner-scoped admission and control for module-owned workflow actors.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use anyhow::{Context, Result, bail};
use tinybus::Connection;
use tinyflows::engine::CancellationToken;
use tinyflows_bus::runs::{
    BootSweep, BootSweepResponse, CancelRunResponse, ExpireParked, ExpireParkedResponse, PollRun,
    ResumeRun, ResumeRunResponse, RunControlRequest, RunEvent, StartRun, StartRunResponse,
};
use tokio::sync::Notify;

mod actor;
mod cancellation;
mod events;
mod preparation;
mod resume;
mod sweeps;

const RESERVING: u8 = 0;
const RUNNING: u8 = 1;
const PARKED: u8 = 2;
const TERMINAL: u8 = 3;

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct OwnerKey {
    destination: String,
    object_path: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct ScopedRunId {
    owner: OwnerKey,
    run_id: String,
}

pub(super) struct ActiveRun {
    key: ScopedRunId,
    pub(super) lease: String,
    token: CancellationToken,
    phase: AtomicU8,
    admitted: Notify,
    #[cfg(test)]
    pub(super) admission_waiting: Notify,
}

static ACTIVE: LazyLock<Mutex<HashMap<ScopedRunId, Arc<ActiveRun>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn owner(storage: &tinyflows_bus::catalog::CatalogStorage) -> OwnerKey {
    OwnerKey {
        destination: storage.callback.destination.clone(),
        object_path: storage.callback.object_path.clone(),
    }
}

fn key(storage: &tinyflows_bus::catalog::CatalogStorage, run_id: &str) -> ScopedRunId {
    ScopedRunId {
        owner: owner(storage),
        run_id: run_id.to_owned(),
    }
}

fn active(key: &ScopedRunId) -> Option<Arc<ActiveRun>> {
    ACTIVE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(key)
        .cloned()
}

fn register(key: ScopedRunId) -> std::result::Result<Arc<ActiveRun>, Arc<ActiveRun>> {
    let mut active = ACTIVE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(existing) = active.get(&key) {
        return Err(Arc::clone(existing));
    }
    let run = Arc::new(ActiveRun {
        key: key.clone(),
        lease: uuid::Uuid::new_v4().to_string(),
        token: CancellationToken::new(),
        phase: AtomicU8::new(RESERVING),
        admitted: Notify::new(),
        #[cfg(test)]
        admission_waiting: Notify::new(),
    });
    active.insert(key, Arc::clone(&run));
    Ok(run)
}

fn reserve_resume(key: ScopedRunId) -> Result<Arc<ActiveRun>> {
    let mut active = ACTIVE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(existing) = active.get(&key) {
        if existing.phase.load(Ordering::Acquire) != PARKED {
            bail!("flow run already has an active lifecycle operation")
        }
        existing.phase.store(TERMINAL, Ordering::Release);
    }
    let run = Arc::new(ActiveRun {
        key: key.clone(),
        lease: uuid::Uuid::new_v4().to_string(),
        token: CancellationToken::new(),
        phase: AtomicU8::new(RESERVING),
        admitted: Notify::new(),
        #[cfg(test)]
        admission_waiting: Notify::new(),
    });
    active.insert(key, Arc::clone(&run));
    Ok(run)
}

fn unregister(run: &Arc<ActiveRun>) {
    run.phase.store(TERMINAL, Ordering::Release);
    let mut active = ACTIVE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if active
        .get(&run.key)
        .is_some_and(|current| Arc::ptr_eq(current, run))
    {
        active.remove(&run.key);
        run.admitted.notify_waiters();
    }
}

pub(super) fn active_lease(lease: &str) -> Option<Arc<ActiveRun>> {
    ACTIVE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .values()
        .find(|run| run.lease == lease)
        .cloned()
}

/// Validates synchronously, inserts a row and registers its cancellation token,
/// then starts exactly one actor before returning the host-chosen id.
pub(super) async fn start(connection: &Connection, request: StartRun) -> Result<StartRunResponse> {
    let prepared = preparation::prepare(connection, &request).await?;
    let run_key = key(&request.storage, &request.run_id);
    let active = loop {
        if let Some(existing) = active(&run_key) {
            if let Some(response) =
                wait_for_admission(&prepared.catalog, &request.run_id, &run_key, existing).await?
            {
                return Ok(response);
            }
            continue;
        }
        if prepared
            .catalog
            .get_flow_run(&request.run_id)
            .await?
            .is_some()
        {
            bail!("run id already exists in this owner scope")
        }
        match register(run_key.clone()) {
            Ok(active) => break active,
            Err(existing) => {
                if let Some(response) =
                    wait_for_admission(&prepared.catalog, &request.run_id, &run_key, existing)
                        .await?
                {
                    return Ok(response);
                }
            }
        }
    };
    let started_at = chrono::Utc::now().to_rfc3339();
    if let Err(error) = prepared
        .catalog
        .insert_flow_run(
            &request.run_id,
            &request.context.flow_id,
            &request.run_id,
            &started_at,
        )
        .await
    {
        unregister(&active);
        return Err(error).context("reserving flow run row");
    }
    if let Err(error) = prepared.persist_snapshot(&request, &active.lease).await {
        match prepared
            .catalog
            .remove_unstarted_flow_run(&request.run_id)
            .await
        {
            Ok(true) => {}
            Ok(false) | Err(_) => {
                if let Err(settle_error) =
                    finish_admission_failure(&prepared.catalog, &request.run_id, &error.to_string())
                        .await
                {
                    tracing::error!(%settle_error, run_id = %request.run_id, "settling failed run admission");
                }
            }
        }
        unregister(&active);
        active.admitted.notify_waiters();
        return Err(error).context("pinning flow run execution snapshot");
    }
    let (actor_ready, actor_admitted) = tokio::sync::oneshot::channel();
    tokio::spawn(actor::drive(
        connection.clone(),
        request,
        prepared,
        Arc::clone(&active),
        actor_admitted,
    ));
    active.phase.store(RUNNING, Ordering::Release);
    active.admitted.notify_waiters();
    let _ = actor_ready.send(());
    Ok(StartRunResponse {
        run_id: active.key.run_id.clone(),
        already_running: false,
    })
}

async fn finish_admission_failure(
    catalog: &tinyflows_drivers::catalog::FlowCatalogDocuments,
    run_id: &str,
    message: &str,
) -> anyhow::Result<()> {
    let Some(run) = catalog.get_flow_run(run_id).await? else {
        return Ok(());
    };
    if !catalog
        .finish_flow_run(
            run_id,
            "failed",
            &chrono::Utc::now().to_rfc3339(),
            &run.steps,
            &[],
            Some(message),
            None,
        )
        .await?
    {
        return Ok(());
    }
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
                data: serde_json::to_value(events::finished_payload(
                    "failed",
                    Some(message.to_owned()),
                    None,
                ))?,
                graph_run_id: None,
            })?,
        )
        .await?;
    if !catalog.mark_flow_run_terminal_event(run_id, seq).await? {
        anyhow::bail!("failed admission terminal cursor was not committed")
    }
    Ok(())
}

async fn wait_for_admission(
    catalog: &tinyflows_drivers::catalog::FlowCatalogDocuments,
    run_id: &str,
    key: &ScopedRunId,
    existing: Arc<ActiveRun>,
) -> Result<Option<StartRunResponse>> {
    if existing.phase.load(Ordering::Acquire) == RESERVING {
        let admitted = existing.admitted.notified();
        #[cfg(test)]
        existing.admission_waiting.notify_one();
        if existing.phase.load(Ordering::Acquire) == RESERVING {
            admitted.await;
        }
    }
    let is_current = active(key).is_some_and(|current| Arc::ptr_eq(&current, &existing));
    if is_current {
        if catalog
            .get_flow_run(run_id)
            .await?
            .is_some_and(|row| is_terminal(&row.status))
        {
            bail!("run id already exists in this owner scope")
        }
        if existing.phase.load(Ordering::Acquire) == RUNNING
            || existing.phase.load(Ordering::Acquire) == PARKED
        {
            return Ok(Some(StartRunResponse {
                run_id: run_id.to_owned(),
                already_running: true,
            }));
        }
    } else if catalog.get_flow_run(run_id).await?.is_some() {
        bail!("run id already exists in this owner scope")
    }
    Ok(None)
}

fn is_terminal(status: &str) -> bool {
    matches!(
        status,
        "completed" | "completed_with_warnings" | "failed" | "cancelled" | "interrupted"
    )
}

/// Returns a bounded event page; the durable row gates `done` and parked runs
/// remain nonterminal until a later ResumeRun slice settles them.
pub(super) async fn poll(connection: &Connection, request: RunControlRequest) -> Result<PollRun> {
    if request.run_id.trim().is_empty() {
        bail!("run id must not be empty")
    }
    let catalog = preparation::catalog(connection, request.storage.clone()).await?;
    verify_live_owner(&catalog, &request.storage, &request.run_id).await?;
    let run = catalog
        .get_flow_run(&request.run_id)
        .await?
        .context("flow run does not exist in this owner scope")?;
    let values = catalog
        .flow_run_events_after(&request.run_id, request.cursor, 256)
        .await?;
    let last_seq = catalog.flow_run_event_last_seq(&request.run_id).await?;
    if request.cursor > last_seq {
        bail!("run event cursor is ahead of the persisted stream")
    }
    let events: Vec<tinyflows_bus::runs::RunEvent> = values
        .into_iter()
        .map(serde_json::from_value::<tinyflows_bus::runs::RunEvent>)
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("decoding flow run events")?;
    let next_cursor = events.last().map_or(request.cursor, |event| event.seq);
    let terminal_seq = catalog.flow_run_terminal_event_seq(&request.run_id).await?;
    Ok(PollRun {
        events,
        next_cursor,
        done: is_terminal(&run.status)
            && terminal_seq.is_some_and(|seq| next_cursor >= seq && next_cursor >= last_seq),
    })
}

pub(super) async fn resume(
    connection: &Connection,
    request: ResumeRun,
) -> Result<ResumeRunResponse> {
    resume::resume(connection, request).await
}

pub(super) async fn expire_parked(
    connection: &Connection,
    request: ExpireParked,
) -> Result<ExpireParkedResponse> {
    sweeps::expire_parked(connection, request).await
}

pub(super) async fn boot_sweep(
    connection: &Connection,
    request: BootSweep,
) -> Result<BootSweepResponse> {
    sweeps::boot_sweep(connection, request).await
}

pub(super) async fn verify_live_owner(
    catalog: &tinyflows_drivers::catalog::FlowCatalogDocuments,
    storage: &tinyflows_bus::catalog::CatalogStorage,
    run_id: &str,
) -> Result<()> {
    let Some(snapshot) = catalog.flow_run_snapshot(run_id).await? else {
        return Ok(());
    };
    let Some(lease) = snapshot.get("lease").and_then(serde_json::Value::as_str) else {
        return Ok(());
    };
    if let Some(active) = active_lease(lease)
        && active.key != key(storage, run_id)
    {
        bail!("run owner binding does not match the live run")
    }
    Ok(())
}

/// Signals a live run, or uses the row CAS to cancel a parked/orphaned run.
pub(super) async fn cancel(
    connection: &Connection,
    request: RunControlRequest,
) -> Result<CancelRunResponse> {
    if request.run_id.trim().is_empty() {
        bail!("run id must not be empty")
    }
    let scoped = key(&request.storage, &request.run_id);
    if let Some(active) = active(&scoped)
        && (active.phase.load(Ordering::Acquire) == RESERVING
            || active.phase.load(Ordering::Acquire) == RUNNING)
    {
        active.token.cancel();
        return Ok(CancelRunResponse { signalled: true });
    }
    cancellation::cancel_parked_or_stale(connection, request, active(&scoped)).await
}

#[cfg(test)]
#[path = "run_manager_tests.rs"]
mod tests;
