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
                json!({"transactions":self.docs.capabilities().contains(tinystoragedrivers_core::Capability::Transactions)}),
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
            "put" => encode(
                self.docs
                    .put(
                        collection,
                        id,
                        request["doc"].clone(),
                        decode(&request["precondition"])?,
                    )
                    .await?,
            ),
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
