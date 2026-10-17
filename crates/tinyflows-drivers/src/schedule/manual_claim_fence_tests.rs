use super::*;
use async_trait::async_trait;
use chrono::{Duration, Utc};
use serde_json::Value;
use std::sync::Arc;
use tinyflows_schedule::Schedule;
use tinystoragedrivers_core::{
    Capabilities, CollectionSpec, DocumentStore, Filter, MemoryStorage, Page, Precondition, Query,
    Scope, Sort, StorageBackend, Version, Versioned, WriteOp, WriteResult,
};
use tokio::sync::Notify;

#[derive(Debug)]
struct HeldClaimWrite {
    inner: Arc<dyn DocumentStore>,
    entered: Notify,
    release: Notify,
    lose_fence_reply: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl DocumentStore for HeldClaimWrite {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    async fn ensure_collection(
        &self,
        spec: &CollectionSpec,
    ) -> tinystoragedrivers_core::Result<()> {
        self.inner.ensure_collection(spec).await
    }

    async fn get(
        &self,
        collection: &str,
        id: &str,
    ) -> tinystoragedrivers_core::Result<Option<Versioned<Value>>> {
        self.inner.get(collection, id).await
    }

    async fn put(
        &self,
        collection: &str,
        id: &str,
        doc: Value,
        precondition: Precondition,
    ) -> tinystoragedrivers_core::Result<Version> {
        self.inner.put(collection, id, doc, precondition).await
    }

    async fn delete(
        &self,
        collection: &str,
        id: &str,
        precondition: Precondition,
    ) -> tinystoragedrivers_core::Result<bool> {
        self.inner.delete(collection, id, precondition).await
    }

    async fn query(
        &self,
        collection: &str,
        query: &Query,
    ) -> tinystoragedrivers_core::Result<Page<Versioned<Value>>> {
        self.inner.query(collection, query).await
    }

    async fn count(
        &self,
        collection: &str,
        filter: &Filter,
    ) -> tinystoragedrivers_core::Result<u64> {
        self.inner.count(collection, filter).await
    }

    async fn delete_where(
        &self,
        collection: &str,
        filter: &Filter,
    ) -> tinystoragedrivers_core::Result<u64> {
        self.inner.delete_where(collection, filter).await
    }

    async fn claim(
        &self,
        collection: &str,
        filter: &Filter,
        sort: &[Sort],
        patch: &Value,
    ) -> tinystoragedrivers_core::Result<Option<Versioned<Value>>> {
        self.inner.claim(collection, filter, sort, patch).await
    }

    async fn atomic_batch(
        &self,
        ops: Vec<WriteOp>,
    ) -> tinystoragedrivers_core::Result<Vec<WriteResult>> {
        let late_claim = ops.iter().any(|op| match op {
            WriteOp::Put {
                collection, doc, ..
            } => collection == "cron.jobs" && doc["claim_owner"] == "late-owner",
            _ => false,
        });
        let fence = ops.iter().any(|op| match op {
            WriteOp::Put {
                collection, doc, ..
            } => {
                collection == "cron.jobs"
                    && doc["claim_generation"]
                        .as_u64()
                        .is_some_and(|generation| generation > 0)
                    && doc.get("claim_epoch").is_none()
            }
            _ => false,
        });
        if late_claim {
            self.entered.notify_one();
            self.release.notified().await;
        }
        let result = self.inner.atomic_batch(ops).await?;
        if fence
            && self
                .lose_fence_reply
                .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(tinystoragedrivers_core::StorageError::backend(
                "injected lost admission fence response",
            ));
        }
        Ok(result)
    }

    async fn drop_collection(&self, collection: &str) -> tinystoragedrivers_core::Result<()> {
        self.inner.drop_collection(collection).await
    }
}

#[tokio::test]
async fn admission_fence_rejects_claim_transaction_that_commits_after_reconciliation() {
    let backend = MemoryStorage::new();
    let scoped = backend.for_scope(&Scope::local()).unwrap();
    let held_docs = Arc::new(HeldClaimWrite {
        inner: scoped.documents().clone(),
        entered: Notify::new(),
        release: Notify::new(),
        lose_fence_reply: std::sync::atomic::AtomicBool::new(false),
    });
    let store = CronDocuments::new(held_docs.clone());
    let now = Utc::now();
    let job = store
        .add_shell_job(
            None,
            Schedule::At {
                at: now + Duration::hours(1),
            },
            "echo fenced admission",
        )
        .await
        .unwrap();

    let claim_store = store.clone();
    let claim_job = job.clone();
    let late_claim = tokio::spawn(async move {
        claim_store
            .claim_job_now(&claim_job, "late-owner", now, Duration::hours(2))
            .await
    });
    held_docs.entered.notified().await;

    assert!(
        store
            .reconcile_manual_claim_admission(&job, "late-owner", now)
            .await
            .unwrap()
            .is_none()
    );
    held_docs.release.notify_one();
    assert!(late_claim.await.unwrap().unwrap().is_none());

    let current = store.get_job(&job.id).await.unwrap();
    assert_eq!(current.claim_generation, job.claim_generation + 1);
    let current_doc = held_docs
        .inner
        .get("cron.jobs", &job.id)
        .await
        .unwrap()
        .unwrap();
    assert!(current_doc.doc.get("claim_epoch").is_none());
    assert!(store.list_runs(&job.id, 10).await.unwrap().is_empty());
    assert!(
        store
            .claim_job_now(&job, "late-owner", now, Duration::hours(2))
            .await
            .unwrap()
            .is_none(),
        "the stale job snapshot cannot revive the delayed claim"
    );
    let fresh_claim = store
        .claim_job_now(&current, "fresh-owner", now, Duration::hours(2))
        .await
        .unwrap()
        .expect("a fresh snapshot remains admissible after the fence");
    assert!(store.release_claim(&fresh_claim, now).await.unwrap());
}

#[tokio::test]
async fn reconciliation_does_not_fence_changed_snapshots_and_lost_fence_reply_is_idempotent() {
    let backend = MemoryStorage::new();
    let scoped = backend.for_scope(&Scope::local()).unwrap();
    let held_docs = Arc::new(HeldClaimWrite {
        inner: scoped.documents().clone(),
        entered: Notify::new(),
        release: Notify::new(),
        lose_fence_reply: std::sync::atomic::AtomicBool::new(false),
    });
    let store = CronDocuments::new(held_docs.clone());
    let now = Utc::now();
    let captured = store
        .add_shell_job(
            None,
            Schedule::At {
                at: now + Duration::hours(1),
            },
            "echo original",
        )
        .await
        .unwrap();
    let changed = store
        .update_job(
            &captured.id,
            tinyflows_schedule::CronJobPatch {
                command: Some("echo changed".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(
        store
            .reconcile_manual_claim_admission(&captured, "stale-owner", now)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store.get_job(&changed.id).await.unwrap().claim_generation,
        changed.claim_generation
    );

    held_docs
        .lose_fence_reply
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(
        store
            .reconcile_manual_claim_admission(&changed, "fence-owner", now)
            .await
            .is_err()
    );
    let fenced = store.get_job(&changed.id).await.unwrap();
    assert_eq!(fenced.claim_generation, changed.claim_generation + 1);
    assert!(
        store
            .reconcile_manual_claim_admission(&changed, "fence-owner", now)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store.get_job(&changed.id).await.unwrap().claim_generation,
        fenced.claim_generation
    );
}
