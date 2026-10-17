use super::*;
use crate::catalog::FlowCatalogDocuments;
use serde_json::json;
use tinystoragedrivers_core::{
    DocumentStoreExt, MemoryStorage, MigrateOptions, Precondition, Query, Registry, Scope,
    ScopedStorage, StorageBackend, WriteOptions,
};

fn scoped() -> ScopedStorage {
    MemoryStorage::new().for_scope(&Scope::local()).unwrap()
}

async fn seed(storage: &ScopedStorage, collection: &str, id: &str, body: Value) {
    storage
        .documents()
        .put(collection, id, body, Precondition::None)
        .await
        .unwrap();
}

async fn seed_with_created_ms(
    storage: &ScopedStorage,
    collection: &str,
    id: &str,
    body: Value,
    created_ms: i64,
) {
    storage
        .documents()
        .put_with(
            collection,
            id,
            body,
            Precondition::None,
            WriteOptions::new().created_ms(created_ms),
        )
        .await
        .unwrap();
}

async fn migrate(storage: &ScopedStorage) {
    let registry = Registry::compose([crate::catalog_storage_schema()]).unwrap();
    registry
        .migrate(storage, &MigrateOptions::new())
        .await
        .unwrap();
}

fn graph() -> Value {
    json!({"schema_version": 1, "nodes": [], "edges": []})
}

#[test]
fn the_schema_composes_and_names_every_catalog_collection() {
    let schema = crate::catalog_storage_schema();
    assert_eq!(schema.owner.as_str(), "flows");
    let names: Vec<&str> = schema.items.iter().map(|item| item.name()).collect();
    for name in [
        "flows.definitions",
        "flows.revisions",
        "flows.runs",
        "flows.suggestions",
        "flows.drafts",
    ] {
        assert!(names.contains(&name), "{name}");
    }
    assert!(schema.items.iter().all(|i| i.scope() == ScopeClass::Tenant));
    assert!(
        !schema
            .items
            .iter()
            .any(|item| item.name() == "flows.workflows")
    );
    Registry::compose([schema]).unwrap();
}

#[tokio::test]
async fn v1_catalog_records_migrate_to_bare_ms_bodies_and_read_back() {
    let storage = scoped();
    let at = "2026-03-04T05:06:07.250+00:00";
    seed(
        &storage,
        "flows_definitions",
        "f1",
        json!({"name": "n", "graph_json": graph().to_string(), "enabled": true,
               "created_ns": 1, "created_at": at, "updated_at": at,
               "require_approval": false, "last_run_at": at, "last_status": "ok",
               "incarnation": "inc"}),
    )
    .await;
    seed(
        &storage,
        "flows_revisions",
        "r1",
        json!({"flow_id": "f1", "graph_json": graph().to_string(), "name": "old",
               "require_approval": false, "created_at": at, "created_ns": 1,
               "touched_ns": 5_000_000_000_i64, "flow_incarnation": "inc"}),
    )
    .await;
    seed(
        &storage,
        "flows_runs",
        "run1",
        json!({"flow_id": "f1", "thread_id": "t", "status": "success",
               "started_at": at, "started_ns": 1, "finished_at": at, "finished_ns": 2,
               "steps_json": "[{\"node_id\":\"a\",\"output\":1}]",
               "pending_approvals_json": "[\"x\"]", "flow_incarnation": "inc"}),
    )
    .await;
    seed(
        &storage,
        "flows_suggestions",
        "s1",
        json!({"title": "t", "one_liner": "o", "rationale": "r", "steps_json": "[\"a\"]",
               "connections_json": "[]", "slugs_json": "[\"s\"]", "build_prompt": "b",
               "confidence": 0.5, "status": "new", "created_at": at, "created_ns": 1}),
    )
    .await;
    let draft = json!({"id": "d1", "flow_id": "f1", "name": "d", "graph": {"a": 1},
                       "origin": "chat", "created_at": at, "updated_at": at});
    seed(
        &storage,
        "flows_drafts",
        "d1",
        json!({"draft_json": draft.to_string(), "updated_ns": 1}),
    )
    .await;

    migrate(&storage).await;

    let docs = storage.documents();
    let def = docs.get("flows.definitions", "f1").await.unwrap().unwrap();
    assert_eq!(def.doc["graph"], graph(), "the graph string is parsed");
    assert_eq!(def.doc["created_ms"], 1_772_600_767_250_i64);
    assert_eq!(def.doc["last_run_ms"], 1_772_600_767_250_i64);
    assert_eq!(
        def.meta.created_ms, 1_772_600_767_250_i64,
        "history keeps its time"
    );
    for gone in [
        "created_ns",
        "created_at",
        "updated_at",
        "last_run_at",
        "graph_json",
    ] {
        assert!(def.doc.get(gone).is_none(), "{gone}");
    }
    let rev = docs.get("flows.revisions", "r1").await.unwrap().unwrap();
    assert_eq!(rev.doc["touched_ms"], 5000);
    let run = docs.get("flows.runs", "run1").await.unwrap().unwrap();
    assert_eq!(run.doc["steps"][0]["node_id"], "a");
    assert_eq!(run.doc["pending_approvals"], json!(["x"]));
    assert_eq!(run.doc["finished_ms"], 1_772_600_767_250_i64);
    let sug = docs.get("flows.suggestions", "s1").await.unwrap().unwrap();
    assert_eq!(sug.doc["suggested_slugs"], json!(["s"]));
    let d = docs.get("flows.drafts", "d1").await.unwrap().unwrap();
    assert_eq!(d.doc["graph"], json!({"a": 1}));
    assert_eq!(d.doc["origin"], "chat");
    assert_eq!(d.doc["updated_ms"], 1_772_600_767_250_i64);

    // The migrated records read through the catalog.
    let catalog = FlowCatalogDocuments::new(std::sync::Arc::clone(docs));
    let (flows, skipped) = catalog.list_flows().await.unwrap();
    assert_eq!((flows.len(), skipped), (1, 0));
    assert_eq!(flows[0].created_at, "2026-03-04T05:06:07.250+00:00");
    assert_eq!(
        catalog
            .get_flow_run("run1")
            .await
            .unwrap()
            .unwrap()
            .steps
            .len(),
        1
    );
    assert_eq!(catalog.list_revisions("f1", 10).await.unwrap().len(), 1);
    assert_eq!(catalog.list_drafts().await.unwrap().len(), 1);
    assert_eq!(catalog.list_suggestions(None, 10).await.unwrap().len(), 1);
}

#[tokio::test]
async fn v1_revision_sequence_run_events_and_snapshot_migrate_and_read_back() {
    let storage = scoped();
    let at = "2026-03-04T05:06:07.250+00:00";
    seed(
        &storage,
        "flows_definitions",
        "f1",
        json!({"name": "n", "graph_json": graph().to_string(), "enabled": true,
               "created_at": at, "updated_at": at, "require_approval": false,
               "incarnation": "inc", "revision": 4}),
    )
    .await;
    seed(
        &storage,
        "flows_runs",
        "run1",
        json!({"flow_id": "f1", "thread_id": "t", "status": "completed",
               "started_at": at, "finished_at": at, "flow_incarnation": "inc",
               "steps_json": "[]", "pending_approvals_json": "[]",
               "events_json": "[{\"seq\":1,\"kind\":\"started\"},{\"seq\":2,\"kind\":\"finished\",\"data\":{\"status\":\"completed\"}}]",
               "execution_snapshot": {"lease": "actor", "timeout_ms": 5},
               "terminal_event_seq": 2}),
    )
    .await;

    migrate(&storage).await;

    let docs = storage.documents();
    let def = docs.get("flows.definitions", "f1").await.unwrap().unwrap();
    assert_eq!(def.doc["revision"], 4, "the revision sequence is carried");
    let run = docs.get("flows.runs", "run1").await.unwrap().unwrap();
    assert_eq!(
        run.doc["events"][1]["kind"], "finished",
        "events are a real array"
    );
    assert!(run.doc.get("events_json").is_none());
    assert_eq!(run.doc["execution_snapshot"]["lease"], "actor");
    assert_eq!(run.doc["terminal_event_seq"], 2);

    let catalog = FlowCatalogDocuments::new(std::sync::Arc::clone(docs));
    assert_eq!(catalog.latest_revision_number("f1").await.unwrap(), Some(4));
    assert_eq!(catalog.flow_run_event_last_seq("run1").await.unwrap(), 2);
    assert_eq!(
        catalog.flow_run_terminal_event_seq("run1").await.unwrap(),
        Some(2)
    );
    assert_eq!(
        catalog
            .flow_run_events_after("run1", 1, 10)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn a_corrupt_v1_graph_string_stays_a_string_and_migrating_twice_changes_nothing() {
    let storage = scoped();
    seed(
        &storage,
        "flows_definitions",
        "bad",
        json!({"name": "n", "graph_json": "{not json", "enabled": false,
               "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z"}),
    )
    .await;
    migrate(&storage).await;
    migrate(&storage).await;
    let rows = storage
        .documents()
        .query_all("flows.definitions", &Query::all())
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].doc["graph"], "{not json");
    let catalog = FlowCatalogDocuments::new(std::sync::Arc::clone(storage.documents()));
    let (flows, skipped) = catalog.list_flows().await.unwrap();
    assert_eq!((flows.len(), skipped), (0, 1), "still skipped as corrupt");
}

#[tokio::test]
async fn v1_records_missing_their_json_field_are_carried_not_dropped() {
    let storage = scoped();
    seed(
        &storage,
        "flows_definitions",
        "d",
        json!({"name": "no graph"}),
    )
    .await;
    seed(&storage, "flows_revisions", "r", json!({"flow_id": "d"})).await;
    seed(&storage, "flows_drafts", "x", json!({"updated_ns": 5})).await;
    migrate(&storage).await;
    for (collection, id) in [
        ("flows.definitions", "d"),
        ("flows.revisions", "r"),
        ("flows.drafts", "x"),
    ] {
        let row = storage.documents().get(collection, id).await.unwrap();
        assert!(
            row.is_some(),
            "{collection}/{id} must survive the migration"
        );
    }
}

#[tokio::test]
async fn catalog_migration_preserves_source_record_metadata_without_body_timestamps() {
    let storage = scoped();
    let created_ms = 1_700_000_123_456_i64;
    seed_with_created_ms(
        &storage,
        "flows_revisions",
        "legacy-revision",
        json!({"flow_id":"legacy-flow","name":"old","graph_json":"{}","require_approval":false}),
        created_ms,
    )
    .await;

    migrate(&storage).await;

    let revision = storage
        .documents()
        .get("flows.revisions", "legacy-revision")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(revision.doc["created_ms"], created_ms);
    assert_eq!(revision.meta.created_ms, created_ms);
}

#[tokio::test]
async fn catalog_migration_preserves_zero_negative_and_source_creation_times() {
    let storage = scoped();
    let epoch_metadata = 1_700_000_000_000_i64;
    let draft_created_ms = 1_600_000_000_000_i64;
    let draft_updated_ms = 1_800_000_000_000_i64;
    seed_with_created_ms(
        &storage,
        "flows_definitions",
        "epoch-flow",
        json!({"name":"Epoch","graph_json":"{}","enabled":true,
            "created_at":"1970-01-01T00:00:00Z","updated_at":"1970-01-01T00:00:00Z"}),
        epoch_metadata,
    )
    .await;
    seed_with_created_ms(
        &storage,
        "flows_revisions",
        "negative-revision",
        json!({"flow_id":"epoch-flow","name":"Before epoch","graph_json":"{}",
            "require_approval":false,"created_at":"1969-12-31T23:59:59.999Z"}),
        epoch_metadata,
    )
    .await;
    seed_with_created_ms(
        &storage,
        "flows_drafts",
        "older-draft",
        json!({"draft_json":"{\"flow_id\":\"epoch-flow\",\"name\":\"Draft\",\"graph\":{},\"updated_at\":\"2027-01-15T08:00:00Z\"}"}),
        draft_created_ms,
    )
    .await;

    migrate(&storage).await;

    let epoch = storage
        .documents()
        .get("flows.definitions", "epoch-flow")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(epoch.doc["created_ms"], 0);
    assert_eq!(epoch.meta.created_ms, 0);
    let negative = storage
        .documents()
        .get("flows.revisions", "negative-revision")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(negative.doc["created_ms"], -1);
    assert_eq!(negative.meta.created_ms, -1);
    let draft = storage
        .documents()
        .get("flows.drafts", "older-draft")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.doc["created_ms"], draft_created_ms);
    assert_eq!(draft.doc["updated_ms"], draft_updated_ms);
    assert_eq!(draft.meta.created_ms, draft_created_ms);
}
