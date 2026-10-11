//! Mechanical JSON bridge to a scoped document port; cron logic stays upstream.
use async_trait::async_trait;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tinybus::Connection;
use tinyflows_bus::schedule::{CallbackAddress, DOCUMENT_INTERFACE, DOCUMENT_METHOD};
use tinystoragedrivers_core::{
    Capabilities, Capability, CollectionSpec, DocumentStore, Filter, Page, Precondition, Query,
    Result, Sort, StorageError, Version, Versioned, WriteOp, WriteResult,
};

pub(super) struct Documents {
    connection: Connection,
    address: CallbackAddress,
    capabilities: Capabilities,
}

impl std::fmt::Debug for Documents {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScheduleDocuments").finish_non_exhaustive()
    }
}
#[derive(Deserialize)]
#[serde(untagged)]
enum Reply {
    Error { error: Failure },
    Value { value: Value },
}
#[derive(Deserialize)]
struct Failure {
    kind: String,
    message: String,
}
impl Failure {
    fn into_error(self) -> StorageError {
        match self.kind.as_str() {
            "conflict" => StorageError::conflict(self.message),
            "already_exists" => StorageError::already_exists(self.message),
            "not_found" => StorageError::not_found(self.message),
            "invalid_input" => StorageError::invalid_input(self.message),
            "serialization" => StorageError::new(
                tinystoragedrivers_core::ErrorKind::Serialization,
                self.message,
            ),
            "unavailable" => StorageError::unavailable(self.message),
            _ => StorageError::backend(self.message),
        }
    }
}
impl Documents {
    pub(super) async fn new(
        connection: Connection,
        address: CallbackAddress,
    ) -> anyhow::Result<Self> {
        connection.proxy(
            &address.destination,
            &address.object_path,
            DOCUMENT_INTERFACE,
        )?;
        let mut documents = Self {
            connection,
            address,
            capabilities: Capabilities::none(),
        };
        #[derive(Deserialize)]
        struct CapabilityReport {
            transactions: bool,
        }
        let report: CapabilityReport = documents.request(json!({"op":"capabilities"})).await?;
        if report.transactions {
            documents.capabilities = documents.capabilities.with(Capability::Transactions);
        }
        Ok(documents)
    }
    fn validate_collection(collection: &str) -> Result<()> {
        // Only schedule-owned collections can cross this narrow callback seam.
        if ![
            "cron_jobs",
            "cron_runs",
            "cron_counters",
            "cron_job_policies",
        ]
        .contains(&collection)
        {
            return Err(StorageError::invalid_input(
                "collection is outside the schedule port",
            ));
        }
        Ok(())
    }
    async fn call<R: DeserializeOwned>(&self, collection: &str, request: Value) -> Result<R> {
        Self::validate_collection(collection)?;
        self.request(request).await
    }
    async fn request<R: DeserializeOwned>(&self, request: Value) -> Result<R> {
        let proxy = self
            .connection
            .proxy(
                &self.address.destination,
                &self.address.object_path,
                DOCUMENT_INTERFACE,
            )
            .map_err(|e| StorageError::backend(e.to_string()))?;
        let reply: Reply = proxy
            .call(DOCUMENT_METHOD, &(request,))
            .await
            .map_err(|e| StorageError::unavailable(e.to_string()))?;
        match reply {
            Reply::Error { error } => Err(error.into_error()),
            Reply::Value { value } => Ok(serde_json::from_value(value)?),
        }
    }
}
#[async_trait]
impl DocumentStore for Documents {
    fn capabilities(&self) -> Capabilities {
        self.capabilities
    }
    async fn atomic_batch(&self, ops: Vec<WriteOp>) -> Result<Vec<WriteResult>> {
        self.capabilities.require(Capability::Transactions)?;
        for operation in &ops {
            let collection = match operation {
                WriteOp::Put { collection, .. } | WriteOp::Delete { collection, .. } => collection,
            };
            Self::validate_collection(collection)?;
        }
        self.request(json!({"op":"atomic_batch","ops":ops})).await
    }
    async fn ensure_collection(&self, spec: &CollectionSpec) -> Result<()> {
        self.call(&spec.name, json!({"op":"ensure","spec":spec}))
            .await
    }
    async fn get(&self, collection: &str, id: &str) -> Result<Option<Versioned<Value>>> {
        self.call(
            collection,
            json!({"op":"get","collection":collection,"id":id}),
        )
        .await
    }
    async fn put(
        &self,
        collection: &str,
        id: &str,
        doc: Value,
        precondition: Precondition,
    ) -> Result<Version> {
        self.call(collection, json!({"op":"put","collection":collection,"id":id,"doc":doc,"precondition":precondition})).await
    }
    async fn delete(&self, collection: &str, id: &str, precondition: Precondition) -> Result<bool> {
        self.call(
            collection,
            json!({"op":"delete","collection":collection,"id":id,"precondition":precondition}),
        )
        .await
    }
    async fn query(&self, collection: &str, query: &Query) -> Result<Page<Versioned<Value>>> {
        self.call(
            collection,
            json!({"op":"query","collection":collection,"query":query}),
        )
        .await
    }
    async fn count(&self, collection: &str, filter: &Filter) -> Result<u64> {
        self.call(
            collection,
            json!({"op":"count","collection":collection,"filter":filter}),
        )
        .await
    }
    async fn delete_where(&self, collection: &str, filter: &Filter) -> Result<u64> {
        self.call(
            collection,
            json!({"op":"delete_where","collection":collection,"filter":filter}),
        )
        .await
    }
    async fn claim(
        &self,
        _collection: &str,
        _filter: &Filter,
        _sort: &[Sort],
        _patch: &Value,
    ) -> Result<Option<Versioned<Value>>> {
        Err(StorageError::backend(
            "claim is outside the schedule document port",
        ))
    }
    async fn drop_collection(&self, _collection: &str) -> Result<()> {
        Err(StorageError::backend(
            "drop_collection is outside the schedule document port",
        ))
    }
}

#[cfg(test)]
#[path = "schedule_documents_tests.rs"]
mod tests;
