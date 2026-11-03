use super::*;
use tinystoragedrivers_testkit::MemoryBackend as MemoryStorage;

use std::sync::Arc;

use serde_json::{Map, Value, json};
use tinybus::{Connection, broker::Broker, transport::memory::MemoryTransport};
use tinyflows::graph::{Checkpoint, Checkpointer as _};
use tinyflows_bus::catalog::{CallbackAddress, CatalogStorage};
use tinyflows_bus::runs::RunFinished;
use tinyflows_bus::runs::{
    CallbackAddress as CapsCallbackAddress, FlowRunTrigger, RunContext, StartRun,
};
use tinyflows_catalog::FlowRunStep;
use tinyflows_drivers::{DriverCheckpointer, catalog::FlowCatalogDocuments};
use tinyflows_storage::{Scope, StorageBackend};
use tokio::sync::oneshot;

#[tokio::test]
async fn dropping_an_unpolled_actor_future_reconciles_only_its_live_run() {
    let fixture = running_fixture().await;
    let (_admitted, admitted_rx) = oneshot::channel();
    let actor = actor::drive(
        fixture.connection.clone(),
        fixture.request.clone(),
        fixture.prepared,
        Arc::clone(&fixture.active),
        admitted_rx,
    );

    // Tokio can drop a spawned future before its first poll during shutdown.
    // The actor factory must have captured recovery ownership before it
    // returned this future, so the admitted row cannot be left running.
    let (reconciled_tx, reconciled_rx) = oneshot::channel();
    install_recovery_completion(&fixture.run_id, reconciled_tx);
    drop(actor);
    reconciled_rx
        .await
        .expect("actor recovery task must finish");

    assert_reconciled(
        &fixture.catalog,
        &fixture.checkpointer,
        &fixture.flow_id,
        &fixture.run_id,
        &fixture.storage,
    )
    .await;
}

#[tokio::test]
async fn aborting_an_admitted_actor_reconciles_its_running_row() {
    let fixture = running_fixture().await;
    let (arrived_tx, arrived_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    install_after_admission_gate(&fixture.run_id, arrived_tx, release_rx);

    let (admitted_tx, admitted_rx) = oneshot::channel();
    let task = tokio::spawn(actor::drive(
        fixture.connection.clone(),
        fixture.request.clone(),
        fixture.prepared,
        Arc::clone(&fixture.active),
        admitted_rx,
    ));
    admitted_tx.send(()).unwrap();
    arrived_rx.await.unwrap();
    let (reconciled_tx, reconciled_rx) = oneshot::channel();
    install_recovery_completion(&fixture.run_id, reconciled_tx);
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    drop(release_tx);
    reconciled_rx
        .await
        .expect("actor recovery task must finish");

    assert_reconciled(
        &fixture.catalog,
        &fixture.checkpointer,
        &fixture.flow_id,
        &fixture.run_id,
        &fixture.storage,
    )
    .await;
}

#[tokio::test]
async fn a_late_actor_guard_preserves_parked_terminal_and_replacement_runs() {
    let parked = running_fixture().await;
    let (_parked_admission, parked_rx) = oneshot::channel();
    let parked_actor = actor::drive(
        parked.connection.clone(),
        parked.request.clone(),
        parked.prepared,
        Arc::clone(&parked.active),
        parked_rx,
    );
    assert!(
        parked
            .catalog
            .finish_flow_run(
                &parked.run_id,
                "pending_approval",
                "2026-01-01T00:00:01Z",
                &[],
                &["gate".into()],
                None,
                Some("graph-pin"),
            )
            .await
            .unwrap()
    );
    parked
        .active
        .phase
        .store(PARKED, std::sync::atomic::Ordering::Release);
    drop(parked_actor);
    wait_for_no_active(&parked.storage, &parked.run_id).await;
    let row = parked
        .catalog
        .get_flow_run(&parked.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.status, "pending_approval");
    assert!(
        parked
            .catalog
            .flow_run_events_after(&parked.run_id, 0, 10)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        !parked
            .checkpointer
            .get_thread(&parked.run_id)
            .await
            .unwrap()
            .is_empty()
    );

    let terminal = running_fixture().await;
    let (_terminal_admission, terminal_rx) = oneshot::channel();
    let terminal_actor = actor::drive(
        terminal.connection.clone(),
        terminal.request.clone(),
        terminal.prepared,
        Arc::clone(&terminal.active),
        terminal_rx,
    );
    assert!(
        terminal
            .catalog
            .finish_flow_run(
                &terminal.run_id,
                "completed",
                "2026-01-01T00:00:01Z",
                &[FlowRunStep {
                    node_id: "finished".into(),
                    ..Default::default()
                }],
                &[],
                None,
                None,
            )
            .await
            .unwrap()
    );
    drop(terminal_actor);
    wait_for_no_active(&terminal.storage, &terminal.run_id).await;
    let row = terminal
        .catalog
        .get_flow_run(&terminal.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.status, "completed");
    assert_eq!(row.steps[0].node_id, "finished");
    assert!(
        terminal
            .catalog
            .flow_run_events_after(&terminal.run_id, 0, 10)
            .await
            .unwrap()
            .is_empty()
    );

    let replacement = running_fixture().await;
    let (_replacement_admission, replacement_rx) = oneshot::channel();
    let stale_actor = actor::drive(
        replacement.connection.clone(),
        replacement.request.clone(),
        replacement.prepared,
        Arc::clone(&replacement.active),
        replacement_rx,
    );
    assert!(
        replacement
            .catalog
            .finish_flow_run(
                &replacement.run_id,
                "pending_approval",
                "2026-01-01T00:00:01Z",
                &[],
                &["gate".into()],
                None,
                Some("graph-pin"),
            )
            .await
            .unwrap()
    );
    replacement
        .active
        .phase
        .store(PARKED, std::sync::atomic::Ordering::Release);
    let new_actor = reserve_resume(key(&replacement.storage, &replacement.run_id)).unwrap();
    assert!(
        replacement
            .catalog
            .claim_parked_flow_run_for_resume(&replacement.run_id, &new_actor.lease)
            .await
            .unwrap()
    );
    drop(stale_actor);
    wait_for_owner(&replacement.storage, &replacement.run_id, &new_actor).await;
    assert_eq!(
        replacement
            .catalog
            .get_flow_run(&replacement.run_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        "running"
    );
    assert!(
        replacement
            .catalog
            .flow_run_events_after(&replacement.run_id, 0, 10)
            .await
            .unwrap()
            .is_empty()
    );
    unregister(&new_actor);
}

struct RunningFixture {
    _broker: Broker,
    connection: Connection,
    catalog: FlowCatalogDocuments,
    checkpointer: DriverCheckpointer<Value>,
    flow_id: String,
    run_id: String,
    storage: CatalogStorage,
    active: Arc<ActiveRun>,
    prepared: preparation::Prepared,
    request: StartRun,
}

async fn running_fixture() -> RunningFixture {
    let backend = MemoryStorage::new()
        .for_scope(&Scope::new("local").expect("valid scope"))
        .unwrap();
    let docs = Arc::clone(backend.documents());
    let catalog = FlowCatalogDocuments::new(Arc::clone(&docs));
    let graph_json = json!({
        "nodes": [
            {"id":"trigger","name":"Trigger","kind":"trigger","config":{}},
            {"id":"dedup","name":"Dedup","kind":"dedup","config":{"key":"=item.id"}}
        ],
        "edges": [
            {"from_node":"trigger","from_port":"main","to_node":"dedup","to_port":"main"}
        ]
    });
    let graph = tinyflows::migrate::deserialize_graph(graph_json.clone()).unwrap();
    let flow = catalog
        .create_flow("actor recovery".into(), graph.clone(), false, true)
        .await
        .unwrap();
    let run_id = format!("actor-recovery-{}", uuid::Uuid::new_v4());
    let storage = CatalogStorage {
        callback: CallbackAddress {
            destination: "host.documents".into(),
            object_path: format!("/owner/actor-recovery-{}", uuid::Uuid::new_v4()),
        },
    };
    let active = register(key(&storage, &run_id)).unwrap_or_else(|_| panic!("new actor owner"));
    let request = StartRun {
        run_id: run_id.clone(),
        context: RunContext {
            flow_id: flow.id.clone(),
            origin: Some(json!({"kind":"workflow"})),
        },
        caps_host: tinyflows_bus::runs::CapsHost {
            callback: CapsCallbackAddress {
                destination: "host.caps".into(),
                object_path: format!("/caps/{run_id}"),
            },
        },
        storage: storage.clone(),
        graph: graph_json,
        trigger_kind: FlowRunTrigger::Rpc,
        trigger_data: json!({"id":"dedup-item"}),
        inputs: Map::new(),
        timeout_ms: 20_000,
    };
    let started_at = chrono::Utc::now().to_rfc3339();
    catalog
        .insert_flow_run(&run_id, &flow.id, &run_id, &started_at)
        .await
        .unwrap();
    let checkpointer = DriverCheckpointer::<Value>::new(docs);
    let prepared = preparation::Prepared {
        catalog: catalog.clone(),
        compiled: tinyflows::compiler::compile(&graph).unwrap(),
        inputs: Some(Map::new()),
        checkpointer: checkpointer.clone(),
        require_approval: false,
        checkpoint_thread_id: run_id.clone(),
    };
    prepared
        .persist_snapshot(&request, &active.lease)
        .await
        .unwrap();
    active.phase.store(RUNNING, Ordering::Release);

    // Model durable partial state so recovery must clear both the checkpoint
    // and the tentative dedup keys associated with this actor lease.
    let namespace = format!("flow:{}", flow.id);
    catalog
        .kv_set(
            &namespace,
            &tinyflows::nodes::control_flow::dedup::tentative_key("dedup", Some(&run_id)),
            &json!(["dedup-item"]),
        )
        .await
        .unwrap();
    checkpointer
        .put(Checkpoint {
            thread_id: run_id.clone(),
            checkpoint_id: "actor-recovery-checkpoint".into(),
            run_id: Some(run_id.clone()),
            parent_checkpoint_id: None,
            namespace: Vec::new(),
            state: json!({"partial":true}),
            next_nodes: vec![tinyflows::graph::ids::NodeId::new("dedup")],
            completed_tasks: Vec::new(),
            pending_writes: Vec::new(),
            interrupts: Vec::new(),
            pending_activations: None,
            barrier_arrivals: Vec::new(),
            metadata: json!({"source":"loop"}),
        })
        .await
        .unwrap();

    let broker = Broker::new();
    let (client, transport) = MemoryTransport::pair();
    broker.attach(Arc::new(transport));
    let connection = Connection::connect(Box::new(client)).await.unwrap();

    RunningFixture {
        _broker: broker,
        connection,
        catalog,
        checkpointer,
        flow_id: flow.id,
        run_id,
        storage,
        active,
        prepared,
        request,
    }
}

async fn assert_reconciled(
    catalog: &FlowCatalogDocuments,
    checkpointer: &DriverCheckpointer<Value>,
    flow_id: &str,
    run_id: &str,
    storage: &CatalogStorage,
) {
    let mut settled = None;
    // Cleanup runs on other tasks; wait on a wall-clock deadline, not a fixed number of yields,
    // so a loaded CI runner cannot exhaust the budget before the work is scheduled.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        let row = catalog.get_flow_run(run_id).await.unwrap().unwrap();
        let events = catalog.flow_run_events_after(run_id, 0, 20).await.unwrap();
        if row.status == "interrupted"
            && catalog
                .flow_run_terminal_event_seq(run_id)
                .await
                .unwrap()
                .is_some()
        {
            settled = Some((row, events));
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let (row, events) = settled.expect("actor recovery must finish its durable cleanup");
    assert_eq!(row.status, "interrupted");
    assert!(
        row.error
            .as_deref()
            .is_some_and(|error| error.contains("actor"))
    );

    assert_eq!(events.len(), 1, "recovery emits exactly one terminal event");
    assert_eq!(events[0]["kind"], "finished");
    assert_eq!(events[0]["data"]["status"], "interrupted");
    let finished: RunFinished = serde_json::from_value(events[0]["data"].clone()).unwrap();
    assert!(!finished.successful);
    assert!(!finished.timed_out);
    assert!(finished.output.is_none());
    assert_eq!(
        catalog.flow_run_terminal_event_seq(run_id).await.unwrap(),
        events[0]["seq"].as_u64()
    );

    let flow = catalog.get_flow(flow_id).await.unwrap().unwrap();
    assert_eq!(flow.last_status.as_deref(), Some("interrupted"));
    assert!(checkpointer.get_thread(run_id).await.unwrap().is_empty());
    assert_eq!(
        catalog
            .kv_get(
                &format!("flow:{flow_id}"),
                &tinyflows::nodes::control_flow::dedup::tentative_key("dedup", Some(run_id)),
            )
            .await
            .unwrap(),
        None
    );
    assert!(active(&key(storage, run_id)).is_none());
}

async fn wait_for_no_active(storage: &CatalogStorage, run_id: &str) {
    for _ in 0..128 {
        if active(&key(storage, run_id)).is_none() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("the stopped actor stayed registered");
}

async fn wait_for_owner(storage: &CatalogStorage, run_id: &str, expected: &Arc<ActiveRun>) {
    for _ in 0..128 {
        if active(&key(storage, run_id)).is_some_and(|actual| Arc::ptr_eq(&actual, expected)) {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("the stale actor recovery removed or replaced the new owner");
}

#[cfg(test)]
struct AdmissionGate {
    arrived: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
}

#[cfg(test)]
fn install_after_admission_gate(
    run_id: &str,
    arrived: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
) {
    AFTER_ADMISSION_GATES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(run_id.to_owned(), AdmissionGate { arrived, release });
}

#[cfg(test)]
static AFTER_ADMISSION_GATES: LazyLock<Mutex<HashMap<String, AdmissionGate>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[cfg(test)]
static RECOVERY_COMPLETIONS: LazyLock<Mutex<HashMap<String, oneshot::Sender<()>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[cfg(test)]
fn install_recovery_completion(run_id: &str, finished: oneshot::Sender<()>) {
    RECOVERY_COMPLETIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(run_id.to_owned(), finished);
}

#[cfg(test)]
pub(super) fn notify_recovery_finished_for_test(run_id: &str) {
    if let Some(finished) = RECOVERY_COMPLETIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(run_id)
    {
        let _ = finished.send(());
    }
}

#[cfg(test)]
pub(super) async fn pause_after_admission_for_test(run_id: &str) {
    let gate = AFTER_ADMISSION_GATES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(run_id);
    if let Some(gate) = gate {
        let _ = gate.arrived.send(());
        let _ = gate.release.await;
    }
}
