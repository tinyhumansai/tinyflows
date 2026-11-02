use super::*;
use tinystoragedrivers_testkit::MemoryBackend as MemoryStorage;

use std::future::Future as _;
use std::task::Poll;

use tinyflows::caps::StateStore as _;
use tinyflows_bus::catalog::{CallbackAddress, CatalogStorage};
use tinyflows_storage::{Scope, StorageBackend};

fn storage(path: &str) -> CatalogStorage {
    CatalogStorage {
        callback: CallbackAddress {
            destination: "host.documents".into(),
            object_path: path.into(),
        },
    }
}

fn context(flow_id: &str) -> RunContext {
    RunContext {
        flow_id: flow_id.to_owned(),
        origin: None,
    }
}

fn graph_with_dedup() -> WorkflowGraph {
    tinyflows::migrate::deserialize_graph(serde_json::json!({
        "nodes": [{"id":"dedup","name":"Dedup","kind":"dedup","config":{"key":"=item.id"}}],
        "edges": []
    }))
    .expect("dedup graph")
}

async fn catalog() -> FlowCatalogDocuments {
    let backend = MemoryStorage::new()
        .for_scope(&Scope::new("local").expect("valid scope"))
        .unwrap();
    let catalog = FlowCatalogDocuments::new(backend.documents().clone());
    catalog.ensure().await.unwrap();
    catalog
}

#[tokio::test]
async fn same_owner_and_flow_settlement_lease_is_mutually_exclusive_but_other_keys_progress() {
    let graph = graph_with_dedup();
    let same_owner = storage("/owner/alice");
    let other_owner = storage("/owner/bob");
    let first_context = context("flow-a");
    let first =
        DedupSettlementScope::acquire(&same_owner, &first_context, &graph, "run-first").await;

    // Poll a genuine production acquire future once. A same-key call must be
    // pending while the first RAII scope lives; this is deterministic and does
    // not rely on sleeps or a two-reader barrier.
    let mut second = Box::pin(DedupSettlementScope::acquire(
        &same_owner,
        &first_context,
        &graph,
        "run-second",
    ));
    let first_poll = std::future::poll_fn(|cx| Poll::Ready(second.as_mut().poll(cx))).await;
    assert!(
        first_poll.is_pending(),
        "same owner and flow must serialize"
    );

    let other_owner_scope =
        DedupSettlementScope::acquire(&other_owner, &first_context, &graph, "run-bob").await;
    let other_flow_scope =
        DedupSettlementScope::acquire(&same_owner, &context("flow-b"), &graph, "run-other-flow")
            .await;
    drop(other_owner_scope);
    drop(other_flow_scope);

    drop(first);
    let second = second.await;
    drop(second);
}

#[tokio::test]
async fn real_catalog_settlements_preserve_sequential_union_and_failure_release() {
    let catalog = catalog().await;
    let flow_id = format!("dedup-settlement-{}", uuid::Uuid::new_v4());
    let storage = storage("/owner/catalog");
    let context = context(&flow_id);
    let graph = graph_with_dedup();
    let state = FlowStateDocuments::open(catalog.clone(), format!("flow:{flow_id}"))
        .await
        .unwrap();

    state
        .store("dedup:dedup:tentative:run-a", serde_json::json!(["a"]))
        .await
        .unwrap();
    let first = DedupSettlementScope::acquire(&storage, &context, &graph, "run-a").await;
    first.settle(catalog.clone(), true).await;
    drop(first);

    state
        .store("dedup:dedup:tentative:run-b", serde_json::json!(["b"]))
        .await
        .unwrap();
    let second = DedupSettlementScope::acquire(&storage, &context, &graph, "run-b").await;
    second.settle(catalog.clone(), true).await;
    drop(second);

    let committed = state
        .load("dedup:dedup:committed")
        .await
        .unwrap()
        .expect("successful module settlements commit the key set");
    assert_eq!(committed, serde_json::json!(["a", "b"]));
    assert_eq!(
        state.load("dedup:dedup:tentative:run-b").await.unwrap(),
        None
    );

    state
        .store(
            "dedup:dedup:tentative:run-failed",
            serde_json::json!(["retry-me"]),
        )
        .await
        .unwrap();
    let failed = DedupSettlementScope::acquire(&storage, &context, &graph, "run-failed").await;
    failed.settle(catalog, false).await;
    drop(failed);

    assert_eq!(
        state.load("dedup:dedup:committed").await.unwrap(),
        Some(serde_json::json!(["a", "b"])),
        "a failed actor releases tentative keys without erasing a prior winner"
    );
    assert_eq!(
        state
            .load("dedup:dedup:tentative:run-failed")
            .await
            .unwrap(),
        None
    );
}
