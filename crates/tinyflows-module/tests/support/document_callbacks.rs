//! Test host: delegates only document I/O, without cron knowledge.
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tinystoragedrivers_core::{DocumentStore, Result, StorageError};
fn decode<T: DeserializeOwned>(value: &Value) -> Result<T> {
    Ok(serde_json::from_value(value.clone())?)
}
fn encode<T: Serialize>(value: T) -> Result<Value> {
    Ok(serde_json::to_value(value)?)
}
pub struct Documents {
    pub docs: Arc<dyn DocumentStore>,
    pub conflict_once: Arc<AtomicBool>,
    pub atomic_calls: Arc<AtomicUsize>,
}
#[tinybus::interface(name = "ai.tinyhumans.tinyflows.ScheduleDocuments")]
impl Documents {
    async fn document(&self, request: Value) -> tinybus::Result<Value> {
        let result = self.apply(request).await;
        Ok(match result {
            Ok(value) => json!({"value":value}),
            Err(error) => {
                json!({"error":{"kind":error.kind().to_string().replace(' ', "_"),"message":error.to_string()}})
            }
        })
    }
}

impl Documents {
    async fn apply(&self, request: Value) -> Result<Value> {
        let collection = request["collection"].as_str().unwrap_or("");
        let id = request["id"].as_str().unwrap_or("");
        match request["op"].as_str().unwrap_or("") {
            "capabilities" => Ok(
                json!({"transactions":self.docs.capabilities().contains(tinystoragedrivers_core::Capability::Transactions),"record_metadata":true}),
            ),
            "atomic_batch" => {
                self.atomic_calls.fetch_add(1, Ordering::SeqCst);
                if self.conflict_once.swap(false, Ordering::SeqCst) {
                    return Err(StorageError::conflict("injected concurrent atomic write"));
                }
                encode(self.docs.atomic_batch(decode(&request["ops"])?).await?)
            }
            "ensure" => {
                self.docs
                    .ensure_collection(&decode(&request["spec"])?)
                    .await?;
                Ok(Value::Null)
            }
            "get" => encode(self.docs.get(collection, id).await?),
            "put" => {
                let mut options = tinystoragedrivers_core::WriteOptions::new();
                if let Some(schema) = request["schema"].as_u64() {
                    options = options.schema(u32::try_from(schema).unwrap_or(0));
                }
                if let Some(expires_ms) = request["expires_ms"].as_i64() {
                    options = options.expires_ms(expires_ms);
                }
                if let Some(created_ms) = request["created_ms"].as_i64() {
                    options = options.created_ms(created_ms);
                }
                encode(
                    self.docs
                        .put_with(
                            collection,
                            id,
                            request["doc"].clone(),
                            decode(&request["precondition"])?,
                            options,
                        )
                        .await?,
                )
            }
            "delete" => encode(
                self.docs
                    .delete(collection, id, decode(&request["precondition"])?)
                    .await?,
            ),
            "query" => encode(
                self.docs
                    .query(collection, &decode(&request["query"])?)
                    .await?,
            ),
            "count" => encode(
                self.docs
                    .count(collection, &decode(&request["filter"])?)
                    .await?,
            ),
            "delete_where" => encode(
                self.docs
                    .delete_where(collection, &decode(&request["filter"])?)
                    .await?,
            ),
            _ => Err(StorageError::invalid_input("unknown document operation")),
        }
    }
}

/// Injects response loss only after the backing transaction has committed.
#[allow(dead_code)]
pub struct CommitThenFail {
    pub docs: Arc<dyn DocumentStore>,
    pub fail_claim_reply: Arc<AtomicBool>,
    pub fail_release_reply: Arc<AtomicBool>,
    pub fail_completion_before_commit: Arc<AtomicBool>,
}

#[allow(dead_code)]
#[tinybus::interface(name = "ai.tinyhumans.tinyflows.ScheduleDocuments")]
impl CommitThenFail {
    async fn document(&self, request: Value) -> tinybus::Result<Value> {
        let claim_commit = request["op"] == "atomic_batch"
            && request["ops"].as_array().is_some_and(|ops| {
                ops.iter().any(|op| {
                    op["op"] == "put"
                        && op["collection"] == "cron.jobs"
                        && op["doc"].get("claim_epoch").is_some()
                        && op["doc"].get("claim_owner").is_some()
                })
            });
        let release_commit = request["op"] == "atomic_batch"
            && request["ops"].as_array().is_some_and(|ops| {
                ops.iter().any(|op| {
                    op["op"] == "put"
                        && op["collection"] == "cron.jobs"
                        && op["doc"].get("claim_epoch").is_some()
                        && op["doc"].get("completed_occurrence").is_none()
                        && op["doc"].get("queued_run").is_none()
                }) && ops
                    .iter()
                    .any(|op| op["op"] == "delete" && op["collection"] == "cron.runs")
            });
        let completion_commit = request["op"] == "atomic_batch"
            && request["ops"].as_array().is_some_and(|ops| {
                ops.iter().any(|op| {
                    op["op"] == "put"
                        && op["collection"] == "cron.jobs"
                        && op["doc"].get("completed_occurrence").is_some()
                })
            });
        if completion_commit
            && self
                .fail_completion_before_commit
                .swap(false, Ordering::SeqCst)
        {
            return Err(tinybus::Error::failed("injected terminal commit failure"));
        }
        let result = Documents {
            docs: self.docs.clone(),
            conflict_once: Arc::new(AtomicBool::new(false)),
            atomic_calls: Arc::new(AtomicUsize::new(0)),
        }
        .document(request)
        .await?;
        if claim_commit && self.fail_claim_reply.swap(false, Ordering::SeqCst) {
            return Err(tinybus::Error::failed(
                "injected lost claim reply after commit",
            ));
        }
        if release_commit && self.fail_release_reply.swap(false, Ordering::SeqCst) {
            return Err(tinybus::Error::failed(
                "injected lost release reply after commit",
            ));
        }
        Ok(result)
    }
}

/// Presents a backend without transaction capability while retaining its
/// ordinary document behavior for fail-closed admission tests.
#[allow(dead_code)]
pub struct NoTransactions {
    pub docs: Arc<dyn DocumentStore>,
}

#[allow(dead_code)]
#[tinybus::interface(name = "ai.tinyhumans.tinyflows.ScheduleDocuments")]
impl NoTransactions {
    async fn document(&self, request: Value) -> tinybus::Result<Value> {
        let capabilities = request["op"] == "capabilities";
        let mut result = Documents {
            docs: self.docs.clone(),
            conflict_once: Arc::new(AtomicBool::new(false)),
            atomic_calls: Arc::new(AtomicUsize::new(0)),
        }
        .document(request)
        .await?;
        if capabilities {
            result["value"]["transactions"] = Value::Bool(false);
        }
        Ok(result)
    }
}
