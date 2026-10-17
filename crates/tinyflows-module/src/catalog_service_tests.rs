use super::*;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tinyflows_bus::catalog::{
    self, CallbackAddress, CatalogStorage, GetRevisionRequest, ListRevisionsRequest,
    PutWorkflowRequest, PutWorkflowResponse,
};
use tinystoragedrivers_core::{
    CollectionSpec, DocumentStore, Key, MemoryStorage, Precondition, Scope, StorageBackend,
    WriteOptions,
};

pub(super) struct HostDocuments {
    pub(super) documents: Arc<dyn DocumentStore>,
    pub(super) fail_draft_deletes: bool,
    pub(super) record_metadata: bool,
    pub(super) fail_revision_put: Arc<AtomicBool>,
}

#[tinybus::interface(name = "ai.tinyhumans.tinyflows.CatalogStorage")]
impl HostDocuments {
    async fn doc_capabilities(&self) -> tinybus::Result<Value> {
        Ok(reply(Ok(json!({"record_metadata":self.record_metadata}))))
    }

    async fn doc_ensure(&self, request: Value) -> tinybus::Result<Value> {
        Ok(reply(
            self.documents
                .ensure_collection(&decode(&request["spec"])?)
                .await,
        ))
    }

    async fn doc_get(&self, request: Value) -> tinybus::Result<Value> {
        Ok(reply(
            self.documents
                .get(text(&request, "collection"), text(&request, "id"))
                .await,
        ))
    }

    async fn doc_put(&self, request: Value) -> tinybus::Result<Value> {
        if request["collection"] == "flows.revisions"
            && self.fail_revision_put.swap(false, Ordering::SeqCst)
        {
            return Ok(reply(Err::<(), _>(
                tinystoragedrivers_core::StorageError::unavailable(
                    "injected revision migration failure",
                ),
            )));
        }
        let mut options = WriteOptions::new();
        if let Some(schema) = request["schema"].as_u64() {
            options = options.schema(u32::try_from(schema).unwrap_or(0));
        }
        if let Some(created_ms) = request["created_ms"].as_i64() {
            options = options.created_ms(created_ms);
        }
        let result = self
            .documents
            .put_with(
                text(&request, "collection"),
                text(&request, "id"),
                request["doc"].clone(),
                decode(&request["precondition"])?,
                options,
            )
            .await;
        Ok(reply(result))
    }

    async fn doc_delete(&self, request: Value) -> tinybus::Result<Value> {
        let result = if self.fail_draft_deletes && text(&request, "collection") == "flows.drafts" {
            Err(tinystoragedrivers_core::StorageError::unavailable(
                "injected draft delete failure",
            ))
        } else {
            self.documents
                .delete(
                    text(&request, "collection"),
                    text(&request, "id"),
                    decode(&request["precondition"])?,
                )
                .await
        };
        Ok(reply(result))
    }

    async fn doc_query(&self, request: Value) -> tinybus::Result<Value> {
        Ok(reply(
            self.documents
                .query(text(&request, "collection"), &decode(&request["query"])?)
                .await,
        ))
    }

    async fn doc_delete_where(&self, request: Value) -> tinybus::Result<Value> {
        Ok(reply(
            self.documents
                .delete_where(text(&request, "collection"), &decode(&request["filter"])?)
                .await,
        ))
    }
}

fn text<'a>(value: &'a Value, field: &str) -> &'a str {
    value[field].as_str().expect("well-formed document request")
}

fn decode<T: DeserializeOwned>(value: &Value) -> tinybus::Result<T> {
    serde_json::from_value(value.clone()).map_err(tinybus::Error::from)
}

fn reply(value: tinystoragedrivers_core::Result<impl Serialize>) -> Value {
    match value {
        Ok(value) => json!({"value":serde_json::to_value(value).unwrap()}),
        Err(error) => {
            json!({"error":{"kind":error.kind().to_string().replace(' ', "_"),"message":error.to_string()}})
        }
    }
}

pub(super) fn graph(trigger_kind: &str, effect: bool) -> Value {
    let mut nodes = vec![
        json!({"id":"trigger","name":"Trigger","kind":"trigger","config":{"trigger_kind":trigger_kind}}),
        json!({"id":"agent","name":"Agent","kind":"agent","config":{"prompt":"Do the work"}}),
    ];
    let mut edges =
        vec![json!({"from_node":"trigger","from_port":"main","to_node":"agent","to_port":"main"})];
    if effect {
        nodes.push(json!({"id":"send","name":"Send","kind":"tool_call","config":{"slug":"provider:send","args":{}}}));
        edges.push(
            json!({"from_node":"agent","from_port":"main","to_node":"send","to_port":"main"}),
        );
    }
    json!({"nodes":nodes,"edges":edges})
}

pub(super) async fn connections() -> (Connection, Connection) {
    let bus = tinybus::transport::memory::MemoryBus::new();
    tinybus::broker::Broker::new().spawn(bus.clone());
    let server = Connection::connect(bus.connect().await.unwrap())
        .await
        .unwrap();
    crate::service::setup(server.clone()).await.unwrap();
    let host = Connection::connect(bus.connect().await.unwrap())
        .await
        .unwrap();
    host.request_name("test.CatalogStorage").await.unwrap();
    (server, host)
}

pub(super) async fn scope(host: &Connection, name: &str) -> CatalogStorage {
    scope_with_documents(host, name).await.0
}

pub(super) async fn scope_with_documents(
    host: &Connection,
    name: &str,
) -> (CatalogStorage, Arc<dyn DocumentStore>) {
    let (storage, docs, _) = scope_with_test_options(host, name, true, false).await;
    (storage, docs)
}

async fn scope_with_test_options(
    host: &Connection,
    name: &str,
    record_metadata: bool,
    fail_revision_once: bool,
) -> (CatalogStorage, Arc<dyn DocumentStore>, Arc<AtomicBool>) {
    let scope = Scope::new(name).unwrap();
    let scoped = MemoryStorage::new().for_scope(&scope).unwrap();
    let docs = scoped.documents().clone();
    let path = format!("/test/catalog/{name}");
    let fail_revision_put = Arc::new(AtomicBool::new(fail_revision_once));
    host.serve_at(
        path.parse().unwrap(),
        HostDocuments {
            documents: docs.clone(),
            fail_draft_deletes: false,
            record_metadata,
            fail_revision_put: fail_revision_put.clone(),
        },
    )
    .await
    .unwrap();
    (
        CatalogStorage {
            callback: CallbackAddress {
                destination: "test.CatalogStorage".into(),
                object_path: path,
            },
        },
        docs,
        fail_revision_put,
    )
}

pub(super) async fn scope_with_draft_delete_failure(
    host: &Connection,
    name: &str,
) -> (CatalogStorage, Arc<dyn DocumentStore>) {
    let scope = Scope::new(name).unwrap();
    let scoped = MemoryStorage::new().for_scope(&scope).unwrap();
    let docs = scoped.documents().clone();
    let path = format!("/test/catalog/{name}");
    host.serve_at(
        path.parse().unwrap(),
        HostDocuments {
            documents: docs.clone(),
            fail_draft_deletes: true,
            record_metadata: true,
            fail_revision_put: Arc::new(AtomicBool::new(false)),
        },
    )
    .await
    .unwrap();
    (
        CatalogStorage {
            callback: CallbackAddress {
                destination: "test.CatalogStorage".into(),
                object_path: path,
            },
        },
        docs,
    )
}

pub(super) fn put_proxy(host: &Connection) -> tinybus::Result<tinybus::Proxy> {
    host.proxy(
        tinyflows_bus::BUS_NAME,
        catalog::OBJECT_PATH,
        catalog::INTERFACE,
    )
}

#[tokio::test]
async fn catalog_bus_serves_create_update_conflict_history_and_agent_scopes() {
    let (_server, host) = connections().await;
    let alice = scope(&host, "alice").await;
    let bob = scope(&host, "bob").await;
    let proxy = put_proxy(&host).unwrap();

    let created: PutWorkflowResponse = proxy
        .call(
            "PutWorkflow",
            &(PutWorkflowRequest::Create {
                storage: alice.clone(),
                name: "Manual flow".into(),
                graph: graph("manual", false),
                require_approval: false,
            }),
        )
        .await
        .unwrap();
    let PutWorkflowResponse::Ok(created) = created else {
        panic!("create failed: {created:?}")
    };
    assert!(!created.born_disabled);
    assert!(!created.approval_forced);
    let id = created.workflow.id.clone();
    let version = created.workflow.updated_at.clone();

    let changed: PutWorkflowResponse = proxy
        .call(
            "PutWorkflow",
            &(PutWorkflowRequest::Update {
                storage: alice.clone(),
                id: id.clone(),
                name: "Updated flow".into(),
                graph: graph("manual", false),
                require_approval: false,
                expected_version: Some(version.clone()),
                force_disarm_automatic: false,
            }),
        )
        .await
        .unwrap();
    let PutWorkflowResponse::Ok(changed) = changed else {
        panic!("update failed: {changed:?}")
    };
    assert_eq!(changed.workflow.name, "Updated flow");
    let conflict: PutWorkflowResponse = proxy
        .call(
            "PutWorkflow",
            &(PutWorkflowRequest::Update {
                storage: alice.clone(),
                id: id.clone(),
                name: "Stale edit".into(),
                graph: graph("manual", false),
                require_approval: false,
                expected_version: Some(version),
                force_disarm_automatic: false,
            }),
        )
        .await
        .unwrap();
    assert!(
        matches!(conflict, PutWorkflowResponse::Error(catalog::FlowUpdateError::VersionConflict { ref current, .. }) if current.name == "Updated flow")
    );

    let revisions: Vec<WorkflowRevision> = proxy
        .call(
            "ListRevisions",
            &(ListRevisionsRequest {
                storage: alice.clone(),
                flow_id: id.clone(),
                limit: 10,
            }),
        )
        .await
        .unwrap();
    assert_eq!(revisions.len(), 1);
    let fetched: Option<WorkflowRevision> = proxy
        .call(
            "GetRevision",
            &(GetRevisionRequest {
                storage: alice.clone(),
                flow_id: id.clone(),
                revision_id: revisions[0].id.clone(),
            }),
        )
        .await
        .unwrap();
    assert_eq!(fetched, Some(revisions[0].clone()));

    let fetched_workflow: Option<Workflow> = proxy
        .call("GetWorkflow", &(alice.clone(), &id))
        .await
        .unwrap();
    assert_eq!(
        fetched_workflow.as_ref().map(|flow| flow.name.as_str()),
        Some("Updated flow")
    );

    let alice_list: ListWorkflowsResult = proxy
        .call("ListWorkflows", &(alice.clone(),))
        .await
        .unwrap();
    assert_eq!(alice_list.workflows[0].name, "Updated flow");
    assert_eq!(alice_list.skipped_rows, 0);
    assert_eq!(alice_list.workflows[0].revision, 2);
    assert_eq!(
        alice_list.workflows[0].trigger_kind.as_deref(),
        Some("manual")
    );
    assert_eq!(alice_list.workflows[0].node_count, 2);
    let bob_list: ListWorkflowsResult = proxy.call("ListWorkflows", &(bob,)).await.unwrap();
    assert!(bob_list.workflows.is_empty());

    let enabled: Workflow = proxy
        .call(
            "SetEnabled",
            &(catalog::SetEnabledRequest {
                storage: alice.clone(),
                id: id.clone(),
                enabled: true,
            }),
        )
        .await
        .unwrap();
    assert!(enabled.enabled);
    let duplicate: PutWorkflowResponse = proxy
        .call(
            "PutWorkflow",
            &(PutWorkflowRequest::Duplicate {
                storage: alice.clone(),
                id: id.clone(),
                name: "Updated flow copy".into(),
            }),
        )
        .await
        .unwrap();
    let PutWorkflowResponse::Ok(duplicate) = duplicate else {
        panic!("duplicate failed: {duplicate:?}")
    };
    assert!(duplicate.born_disabled);
    assert!(!duplicate.workflow.enabled);
    let deleted: catalog::DeleteWorkflowResult = proxy
        .call("DeleteWorkflow", &(alice, &duplicate.workflow.id))
        .await
        .unwrap();
    assert!(deleted.removed);
}

#[path = "catalog_service_migration_tests.rs"]
mod migration_tests;

#[tokio::test]
async fn catalog_create_reports_disabled_and_forced_approval_safety_rules() {
    let (_server, host) = connections().await;
    let storage = scope(&host, "policy").await;
    let proxy = put_proxy(&host).unwrap();
    let result: PutWorkflowResponse = proxy
        .call(
            "PutWorkflow",
            &(PutWorkflowRequest::Create {
                storage,
                name: "Scheduled side effect".into(),
                graph: graph("schedule", true),
                require_approval: false,
            }),
        )
        .await
        .unwrap();
    let PutWorkflowResponse::Ok(result) = result else {
        panic!("create failed: {result:?}")
    };
    assert!(result.born_disabled);
    assert!(result.approval_forced);
    assert!(!result.workflow.enabled);
    assert!(result.workflow.require_approval);
}

#[tokio::test]
async fn catalog_list_reports_corrupt_rows_it_skips() {
    let (_server, host) = connections().await;
    let (storage, docs) = scope_with_documents(&host, "corrupt").await;
    docs.ensure_collection(&CollectionSpec::new("flows.definitions"))
        .await
        .unwrap();
    docs.put(
        "flows.definitions",
        "broken-flow",
        json!({"name":"Broken","graph":"not-a-graph","created_ms":1_767_225_600_000_i64}),
        Precondition::Absent,
    )
    .await
    .unwrap();

    let proxy = put_proxy(&host).unwrap();
    let result: ListWorkflowsResult = proxy.call("ListWorkflows", &(storage,)).await.unwrap();
    assert!(result.workflows.is_empty());
    assert_eq!(result.skipped_rows, 1);
}
