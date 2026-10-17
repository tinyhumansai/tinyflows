//! Mechanical JSON bridge to a scoped document port; cron logic stays upstream.
use async_trait::async_trait;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tinybus::Connection;
use tinyflows_bus::schedule::{CallbackAddress, DOCUMENT_INTERFACE, DOCUMENT_METHOD};
use tinystoragedrivers_core::{
    Capabilities, Capability, CollectionSpec, DocumentStore, Filter, Page, Precondition, Query,
    Result, Sort, StorageError, Version, Versioned, WriteOp, WriteOptions, WriteResult,
};

/// Collections owned by `tsd`, shared by every owner's primitives.
const SHARED: [&str; 3] = ["tsd.counters", "tsd.leases", "tsd.migrations"];
/// Every `cron` record in a shared collection is keyed `Key::of(["cron", ..])`.
const CRON_KEY_PREFIX: &str = "cron/";
const CRON_MIGRATION_KEYS: [&str; 4] = [
    "cron/cron.jobs/jobs_v2",
    "cron/cron.runs/runs_v2",
    "cron/cron.admission/admission_v2",
    "cron/counters_import",
];

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
            #[serde(default)]
            record_metadata: bool,
        }
        let report: CapabilityReport = documents.request(json!({"op":"capabilities"})).await?;
        if report.transactions {
            documents.capabilities = documents.capabilities.with(Capability::Transactions);
        }
        if report.record_metadata {
            documents.capabilities = documents.capabilities.with(Capability::RecordMetadata);
        }
        Ok(documents)
    }
    pub(super) fn require_record_metadata(&self) -> anyhow::Result<()> {
        if self.capabilities.contains(Capability::RecordMetadata) {
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "schedule migration requires record metadata support"
            ))
        }
    }
    fn validate_collection(collection: &str) -> Result<()> {
        // Only schedule-owned collections can cross this narrow callback seam.
        if ![
            "cron.jobs",
            "cron.runs",
            "cron.admission",
            "tsd.counters",
            "tsd.leases",
            "cron_job_policies",
            "cron_jobs",
            "cron_runs",
            "cron_counters",
            "tsd.migrations",
            "tsd.kv",
        ]
        .contains(&collection)
        {
            return Err(StorageError::invalid_input(
                "collection is outside the schedule port",
            ));
        }
        Ok(())
    }
    /// `tsd.counters` and `tsd.leases` are shared with other owners, so a
    /// schedule request may only touch records under its own `cron/` keys and
    /// never scan them.
    fn validate_record(collection: &str, id: &str) -> Result<()> {
        Self::validate_collection(collection)?;
        if collection == "tsd.kv" {
            return Err(StorageError::invalid_input(
                "the schedule port may only ensure tsd.kv for primitive preparation",
            ));
        }
        if collection == "tsd.migrations" && !CRON_MIGRATION_KEYS.contains(&id) {
            return Err(StorageError::invalid_input(
                "record is outside the schedule migration progress keys",
            ));
        }
        if SHARED.contains(&collection) && !id.starts_with(CRON_KEY_PREFIX) {
            return Err(StorageError::invalid_input(
                "record is outside the schedule owner's keys",
            ));
        }
        Ok(())
    }
    fn validate_set(collection: &str) -> Result<()> {
        Self::validate_collection(collection)?;
        if SHARED.contains(&collection) || collection == "tsd.kv" {
            return Err(StorageError::invalid_input(
                "set operations are outside the schedule port for shared collections",
            ));
        }
        Ok(())
    }
    async fn call<R: DeserializeOwned>(&self, collection: &str, request: Value) -> Result<R> {
        Self::validate_collection(collection)?;
        self.request(request).await
    }
    async fn call_record<R: DeserializeOwned>(
        &self,
        collection: &str,
        id: &str,
        request: Value,
    ) -> Result<R> {
        Self::validate_record(collection, id)?;
        self.request(request).await
    }
    async fn call_set<R: DeserializeOwned>(&self, collection: &str, request: Value) -> Result<R> {
        Self::validate_set(collection)?;
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
            let (collection, id) = match operation {
                WriteOp::Put { collection, id, .. } | WriteOp::Delete { collection, id, .. } => {
                    (collection, id)
                }
            };
            Self::validate_record(collection, id)?;
        }
        self.request(json!({"op":"atomic_batch","ops":ops})).await
    }
    async fn ensure_collection(&self, spec: &CollectionSpec) -> Result<()> {
        self.call(&spec.name, json!({"op":"ensure","spec":spec}))
            .await
    }
    async fn get(&self, collection: &str, id: &str) -> Result<Option<Versioned<Value>>> {
        self.call_record(
            collection,
            id,
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
        self.call_record(collection, id, json!({"op":"put","collection":collection,"id":id,"doc":doc,"precondition":precondition})).await
    }
    async fn put_with(
        &self,
        collection: &str,
        id: &str,
        doc: Value,
        precondition: Precondition,
        options: WriteOptions,
    ) -> Result<Version> {
        let mut request = json!({"op":"put","collection":collection,"id":id,"doc":doc,"precondition":precondition});
        if options.schema != 0 {
            request["schema"] = json!(options.schema);
        }
        if let Some(expires_ms) = options.expires_ms {
            request["expires_ms"] = json!(expires_ms);
        }
        if let Some(created_ms) = options.created_ms {
            request["created_ms"] = json!(created_ms);
        }
        self.call_record(collection, id, request).await
    }
    async fn delete(&self, collection: &str, id: &str, precondition: Precondition) -> Result<bool> {
        self.call_record(
            collection,
            id,
            json!({"op":"delete","collection":collection,"id":id,"precondition":precondition}),
        )
        .await
    }
    async fn query(&self, collection: &str, query: &Query) -> Result<Page<Versioned<Value>>> {
        self.call_set(
            collection,
            json!({"op":"query","collection":collection,"query":query}),
        )
        .await
    }
    async fn count(&self, collection: &str, filter: &Filter) -> Result<u64> {
        self.call_set(
            collection,
            json!({"op":"count","collection":collection,"filter":filter}),
        )
        .await
    }
    async fn delete_where(&self, collection: &str, filter: &Filter) -> Result<u64> {
        self.call_set(
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
