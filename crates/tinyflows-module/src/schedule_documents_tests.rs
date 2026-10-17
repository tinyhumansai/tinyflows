use super::*;

use std::sync::Arc;
use tinystoragedrivers_core::{
    Capability, ErrorKind, MemoryStorage, Scope, StorageBackend, WriteOp,
};

struct Callback(Arc<dyn DocumentStore>);
#[tinybus::interface(name = "ai.tinyhumans.tinyflows.ScheduleDocuments")]
impl Callback {
    async fn document(&self, request: Value) -> tinybus::Result<Value> {
        let value = match request["op"].as_str().unwrap() {
            "capabilities" => Ok(
                json!({"transactions": self.0.capabilities().contains(Capability::Transactions)}),
            ),
            "atomic_batch" => self
                .0
                .atomic_batch(serde_json::from_value(request["ops"].clone()).unwrap())
                .await
                .map(|results| serde_json::to_value(results).unwrap()),
            _ => Err(StorageError::invalid_input("unexpected document request")),
        };
        Ok(match value {
            Ok(value) => json!({"value": value}),
            Err(error) => {
                json!({"error":{"kind":error.kind().to_string().replace(' ', "_"),"message":error.to_string()}})
            }
        })
    }
}

#[tokio::test]
async fn document_bridge_preserves_atomic_writes_conflicts_and_collection_scope() {
    let bus = tinybus::transport::memory::MemoryBus::new();
    tinybus::broker::Broker::new().spawn(bus.clone());
    let connection = Connection::connect(bus.connect().await.unwrap())
        .await
        .unwrap();
    let scoped = MemoryStorage::new().for_scope(&Scope::local()).unwrap();
    let storage = scoped.documents().clone();
    storage
        .ensure_collection(&CollectionSpec::new("cron.jobs"))
        .await
        .unwrap();
    storage
        .ensure_collection(&CollectionSpec::new("other_domain"))
        .await
        .unwrap();
    connection
        .serve_at(
            "/test/Documents".try_into().unwrap(),
            Callback(storage.clone()),
        )
        .await
        .unwrap();
    connection.request_name("test.Documents").await.unwrap();
    let documents = Documents::new(
        connection,
        CallbackAddress {
            destination: "test.Documents".into(),
            object_path: "/test/Documents".into(),
        },
    )
    .await
    .unwrap();
    assert!(documents.capabilities().contains(Capability::Transactions));
    let put = |collection: &str, precondition| WriteOp::Put {
        collection: collection.into(),
        id: "job".into(),
        doc: json!({"enabled":true}),
        precondition,
    };
    documents
        .atomic_batch(vec![put("cron.jobs", Precondition::Absent)])
        .await
        .unwrap();
    assert!(storage.get("cron.jobs", "job").await.unwrap().is_some());
    let error = documents
        .atomic_batch(vec![put("cron.jobs", Precondition::Absent)])
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Conflict);
    let error = documents
        .atomic_batch(vec![put("other_domain", Precondition::Absent)])
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidInput);
    assert!(storage.get("other_domain", "job").await.unwrap().is_none());
    let error = documents
        .atomic_batch(vec![
            WriteOp::Put {
                collection: "cron.jobs".into(),
                id: "job".into(),
                doc: json!({"enabled":false}),
                precondition: Precondition::None,
            },
            WriteOp::Delete {
                collection: "other_domain".into(),
                id: "job".into(),
                precondition: Precondition::None,
            },
        ])
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidInput);
    assert_eq!(
        storage.get("cron.jobs", "job").await.unwrap().unwrap().doc["enabled"],
        true
    );
}

struct CapabilityReply {
    reply: Value,
    calls: Arc<std::sync::atomic::AtomicUsize>,
}
#[tinybus::interface(name = "ai.tinyhumans.tinyflows.ScheduleDocuments")]
impl CapabilityReply {
    async fn document(&self, _: Value) -> tinybus::Result<Value> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(self.reply.clone())
    }
}

#[tokio::test]
async fn document_bridge_requires_capability_report_and_never_emulates_transactions() {
    for (reply, supported_report) in [
        (json!({"value":{"transactions":false}}), true),
        (json!({"value":{}}), false),
        (
            json!({"error":{"kind":"unavailable","message":"offline"}}),
            false,
        ),
    ] {
        let bus = tinybus::transport::memory::MemoryBus::new();
        tinybus::broker::Broker::new().spawn(bus.clone());
        let connection = Connection::connect(bus.connect().await.unwrap())
            .await
            .unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        connection
            .serve_at(
                "/test/Capabilities".try_into().unwrap(),
                CapabilityReply {
                    reply,
                    calls: calls.clone(),
                },
            )
            .await
            .unwrap();
        connection.request_name("test.Capabilities").await.unwrap();
        let result = Documents::new(
            connection,
            CallbackAddress {
                destination: "test.Capabilities".into(),
                object_path: "/test/Capabilities".into(),
            },
        )
        .await;
        if supported_report {
            let documents = result.unwrap();
            assert!(!documents.capabilities().contains(Capability::Transactions));
            assert!(
                documents
                    .atomic_batch(vec![WriteOp::Put {
                        collection: "cron.jobs".into(),
                        id: "job".into(),
                        doc: json!({}),
                        precondition: Precondition::Absent,
                    }])
                    .await
                    .is_err()
            );
        } else {
            assert!(result.is_err());
        }
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}

#[test]
fn shared_tsd_collections_admit_only_cron_keys_and_no_scans() {
    assert!(Documents::validate_record("tsd.counters", "cron/runs").is_ok());
    assert!(Documents::validate_record("tsd.leases", "cron/job/123").is_ok());
    assert!(Documents::validate_collection("tsd.kv").is_ok());
    assert!(Documents::validate_record("tsd.kv", "cron/legacy").is_err());
    assert!(Documents::validate_record("tsd.leases", "flows/x").is_err());
    assert!(Documents::validate_record("tsd.counters", "~hashed").is_err());
    assert!(Documents::validate_record("cron.jobs", "anything").is_ok());
    assert!(Documents::validate_record("tsd.migrations", "cron/cron.jobs/jobs_v2").is_ok());
    assert!(Documents::validate_record("tsd.migrations", "cron/other/step").is_err());
    assert!(Documents::validate_record("tsd.migrations", "flows/jobs/jobs_v2").is_err());
    assert!(Documents::validate_set("tsd.migrations").is_err());
    assert!(Documents::validate_set("tsd.leases").is_err());
    assert!(Documents::validate_set("tsd.counters").is_err());
    assert!(Documents::validate_set("tsd.kv").is_err());
    assert!(Documents::validate_set("cron.jobs").is_ok());
}
