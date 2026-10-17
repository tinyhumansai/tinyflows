use super::*;
use tinyflows_bus::catalog::CallbackAddress;
use tinyflows_drivers::catalog::FlowCatalogDocuments;
use tinystoragedrivers_core::{MemoryStorage, Scope, StorageBackend};

fn storage(owner: &str) -> tinyflows_bus::catalog::CatalogStorage {
    tinyflows_bus::catalog::CatalogStorage {
        callback: CallbackAddress {
            destination: "host.documents".into(),
            object_path: format!("/owner/{owner}"),
        },
    }
}

#[tokio::test]
async fn terminal_event_seals_cloned_progress_sinks() {
    let backend = MemoryStorage::new().for_scope(&Scope::local()).unwrap();
    let catalog = FlowCatalogDocuments::new(backend.documents().clone());
    let run_id = format!("sealed-{}", uuid::Uuid::new_v4());
    let graph = tinyflows::migrate::deserialize_graph(serde_json::json!({
        "nodes":[{"id":"trigger","name":"Trigger","kind":"trigger","config":{}}],
        "edges":[]
    }))
    .unwrap();
    let flow = catalog
        .create_flow("Sealed".into(), graph, false, true)
        .await
        .unwrap();
    catalog
        .insert_flow_run(&run_id, &flow.id, &run_id, "2026-01-01T00:00:00Z")
        .await
        .unwrap();
    let writer = events::EventWriter::new(catalog.clone(), run_id.clone(), 0);
    let observer = super::events::Observer::new(writer.clone(), Vec::new());
    let late_progress = observer.progress_sink();
    let terminal_seq = writer
        .emit_terminal(events::finished_payload("completed", None, None))
        .await
        .unwrap();
    late_progress("agent_progress".into(), serde_json::json!({"text":"late"}));
    assert_eq!(writer.flush().await, None);
    assert_eq!(
        catalog.flow_run_event_last_seq(&run_id).await.unwrap(),
        terminal_seq
    );
    let events = catalog.flow_run_events_after(&run_id, 0, 10).await.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["kind"], "finished");
}

#[test]
fn registry_keys_include_the_stable_storage_owner_binding() {
    let run_id = format!("same-id-{}", uuid::Uuid::new_v4());
    let alice = key(&storage("alice"), &run_id);
    let bob = key(&storage("bob"), &run_id);
    assert_ne!(alice, bob);

    let alice_run = match register(alice) {
        Ok(run) => run,
        Err(_) => panic!("alice claims the id"),
    };
    let bob_run = match register(bob) {
        Ok(run) => run,
        Err(_) => panic!("bob may use the same opaque run id"),
    };
    assert!(!Arc::ptr_eq(&alice_run, &bob_run));
    unregister(&alice_run);
    unregister(&bob_run);
}

#[test]
fn duplicate_registration_keeps_the_original_cancellation_owner() {
    let run_id = format!("duplicate-{}", uuid::Uuid::new_v4());
    let key = key(&storage("alice"), &run_id);
    let first = match register(key.clone()) {
        Ok(run) => run,
        Err(_) => panic!("first reservation succeeds"),
    };
    let duplicate = match register(key) {
        Err(run) => run,
        Ok(_) => panic!("duplicate reservation must fail"),
    };
    assert!(Arc::ptr_eq(&first, &duplicate));
    assert!(!first.token.is_cancelled());
    first.token.cancel();
    assert!(duplicate.token.is_cancelled());
    unregister(&first);
}

#[tokio::test]
async fn admission_waiter_rechecks_after_rollback_and_after_success() {
    let backend = MemoryStorage::new().for_scope(&Scope::local()).unwrap();
    let catalog = FlowCatalogDocuments::new(backend.documents().clone());
    let run_id = format!("admission-{}", uuid::Uuid::new_v4());
    let storage = storage("alice");
    let scoped_key = key(&storage, &run_id);
    let reservation = register(scoped_key.clone()).unwrap_or_else(|_| panic!("new reservation"));

    let (finished, mut result) = tokio::sync::oneshot::channel();
    let waiter_catalog = catalog.clone();
    let waiter_key = scoped_key.clone();
    let waiter_run = Arc::clone(&reservation);
    let waiter_run_id = run_id.clone();
    tokio::spawn(async move {
        let result =
            wait_for_admission(&waiter_catalog, &waiter_run_id, &waiter_key, waiter_run).await;
        let _ = finished.send(result);
    });
    reservation.admission_waiting.notified().await;
    assert!(matches!(
        result.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));
    unregister(&reservation);
    assert_eq!(result.await.unwrap().unwrap(), None);

    let flow_graph = tinyflows::migrate::deserialize_graph(serde_json::json!({
        "nodes":[{"id":"trigger","name":"Trigger","kind":"trigger","config":{}}],
        "edges":[]
    }))
    .unwrap();
    let flow = catalog
        .create_flow("Admission".into(), flow_graph, false, true)
        .await
        .unwrap();
    catalog
        .insert_flow_run(&run_id, &flow.id, &run_id, "2026-01-01T00:00:00Z")
        .await
        .unwrap();
    let admitted = register(scoped_key.clone()).unwrap_or_else(|_| panic!("retry reservation"));
    admitted.phase.store(RUNNING, Ordering::Release);
    let duplicate = wait_for_admission(&catalog, &run_id, &scoped_key, Arc::clone(&admitted))
        .await
        .unwrap()
        .expect("the durable row and live token make the retry idempotent");
    assert!(duplicate.already_running);
    unregister(&admitted);
}
