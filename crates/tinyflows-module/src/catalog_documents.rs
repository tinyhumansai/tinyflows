//! Scoped, allowlisted bridge from the catalog driver to the host document port.

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tinybus::Connection;
use tinyflows_bus::catalog::{
    CallbackAddress, CatalogStorageCapabilities, CatalogStorageFailure, CatalogStorageReply,
    DocDeleteRequest, DocDeleteWhereRequest, DocEnsureRequest, DocGetRequest, DocPutRequest,
    DocQueryRequest, STORAGE_INTERFACE, storage,
};
use tinystoragedrivers_core::{
    Capabilities, Capability, CollectionSpec, DocumentStore, Filter, Page, Precondition, Query,
    Result, Sort, StorageError, Version, Versioned, WriteOp, WriteOptions, WriteResult,
};

/// The catalog's own collections (storage schema v2, owner `flows`) and the
/// graph checkpointer's (owner `graph`).
const ALLOWED: [&str; 16] = [
    "flows.definitions",
    "flows.revisions",
    "flows.runs",
    "flows.suggestions",
    "flows.drafts",
    "graph.checkpoints",
    "graph.threads",
    "graph.writes",
    "flows_definitions",
    "flows_revisions",
    "flows_runs",
    "flows_suggestions",
    "flows_drafts",
    "flows_kv",
    "flows_state",
    "tsd.migrations",
];
/// Collections owned by `tsd`, shared by every owner's primitives.
const SHARED: [&str; 4] = ["tsd.kv", "tsd.leases", "tsd.counters", "tsd.migrations"];
/// Every `flows` record in a shared collection is keyed `Key::of(["flows", ..])`.
const FLOWS_KEY_PREFIX: &str = "flows/";
const FLOWS_MIGRATION_KEYS: [&str; 6] = [
    "flows/flows.definitions/definitions_v2",
    "flows/flows.revisions/revisions_v2",
    "flows/flows.runs/runs_v2",
    "flows/flows.suggestions/suggestions_v2",
    "flows/flows.drafts/drafts_v2",
    "flows/kv_import",
];

pub(super) struct Documents {
    connection: Connection,
    address: CallbackAddress,
    capabilities: Capabilities,
}

impl std::fmt::Debug for Documents {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CatalogDocuments").finish_non_exhaustive()
    }
}

fn storage_error(error: CatalogStorageFailure) -> StorageError {
    match error.kind.as_str() {
        "conflict" => StorageError::conflict(error.message),
        "already_exists" => StorageError::already_exists(error.message),
        "not_found" => StorageError::not_found(error.message),
        "invalid_input" => StorageError::invalid_input(error.message),
        "serialization" => StorageError::new(
            tinystoragedrivers_core::ErrorKind::Serialization,
            error.message,
        ),
        "unavailable" => StorageError::unavailable(error.message),
        _ => StorageError::backend(error.message),
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
            STORAGE_INTERFACE,
        )?;
        Ok(Self {
            connection,
            address,
            capabilities: Capabilities::none(),
        })
    }

    pub(super) async fn require_record_metadata(&mut self) -> Result<()> {
        let proxy = self
            .connection
            .proxy(
                &self.address.destination,
                &self.address.object_path,
                STORAGE_INTERFACE,
            )
            .map_err(|error| StorageError::backend(error.to_string()))?;
        let reply: CatalogStorageReply = proxy
            .call(storage::CAPABILITIES, &())
            .await
            .map_err(|error| StorageError::unavailable(error.to_string()))?;
        let report: CatalogStorageCapabilities = match reply {
            CatalogStorageReply::Error { error } => return Err(storage_error(error)),
            CatalogStorageReply::Value { value } => serde_json::from_value(value)?,
        };
        if report.record_metadata {
            self.capabilities = self.capabilities.with(Capability::RecordMetadata);
            Ok(())
        } else {
            Err(StorageError::unsupported(
                Capability::RecordMetadata,
                "catalog migration requires record metadata support",
            ))
        }
    }

    fn validate_collection(collection: &str) -> Result<()> {
        if !ALLOWED.contains(&collection) && !SHARED.contains(&collection) {
            return Err(StorageError::invalid_input(
                "collection is outside the workflow catalog port",
            ));
        }
        Ok(())
    }

    /// `tsd.kv`, `tsd.leases` and `tsd.counters` are shared with other
    /// owners, so a catalog request may only touch records under its own
    /// `flows/` keys.
    fn validate_record(collection: &str, id: &str) -> Result<()> {
        Self::validate_collection(collection)?;
        if collection == "tsd.migrations" && !FLOWS_MIGRATION_KEYS.contains(&id) {
            return Err(StorageError::invalid_input(
                "record is outside the catalog migration progress keys",
            ));
        }
        if SHARED.contains(&collection) && !id.starts_with(FLOWS_KEY_PREFIX) {
            return Err(StorageError::invalid_input(
                "record is outside the workflow catalog owner's keys",
            ));
        }
        Ok(())
    }

    /// Set operations (queries, scans, deletes by filter) are never allowed
    /// over a shared collection.
    fn validate_set(collection: &str) -> Result<()> {
        Self::validate_collection(collection)?;
        if SHARED.contains(&collection) {
            return Err(StorageError::invalid_input(
                "set operations are outside the workflow catalog port for shared collections",
            ));
        }
        Ok(())
    }

    async fn call<R: DeserializeOwned, A: serde::Serialize>(
        &self,
        member: &str,
        request: A,
    ) -> Result<R> {
        let request = serde_json::to_value(request)?;
        if let Some(collection) = request.get("collection").and_then(Value::as_str) {
            Self::validate_collection(collection)?;
        }
        self.request(member, request).await
    }

    async fn call_record<R: DeserializeOwned, A: serde::Serialize>(
        &self,
        member: &str,
        collection: &str,
        id: &str,
        request: A,
    ) -> Result<R> {
        Self::validate_record(collection, id)?;
        self.request(member, request).await
    }

    async fn call_set<R: DeserializeOwned, A: serde::Serialize>(
        &self,
        member: &str,
        collection: &str,
        request: A,
    ) -> Result<R> {
        Self::validate_set(collection)?;
        self.request(member, request).await
    }

    async fn request<R: DeserializeOwned, A: serde::Serialize>(
        &self,
        member: &str,
        request: A,
    ) -> Result<R> {
        let proxy = self
            .connection
            .proxy(
                &self.address.destination,
                &self.address.object_path,
                STORAGE_INTERFACE,
            )
            .map_err(|error| StorageError::backend(error.to_string()))?;
        let reply: CatalogStorageReply = proxy
            .call(member, &(request,))
            .await
            .map_err(|error| StorageError::unavailable(error.to_string()))?;
        match reply {
            CatalogStorageReply::Error { error } => Err(storage_error(error)),
            CatalogStorageReply::Value { value } => Ok(serde_json::from_value(value)?),
        }
    }
}

#[async_trait]
impl DocumentStore for Documents {
    fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    async fn atomic_batch(&self, ops: Vec<WriteOp>) -> Result<Vec<WriteResult>> {
        let _ = ops;
        Err(StorageError::backend(
            "atomic batches are outside the workflow catalog port",
        ))
    }

    async fn ensure_collection(&self, spec: &CollectionSpec) -> Result<()> {
        Self::validate_collection(&spec.name)?;
        self.call(
            storage::ENSURE,
            DocEnsureRequest {
                spec: serde_json::from_value(serde_json::to_value(spec)?)?,
            },
        )
        .await
    }

    async fn get(&self, collection: &str, id: &str) -> Result<Option<Versioned<Value>>> {
        self.call_record(
            storage::GET,
            collection,
            id,
            DocGetRequest {
                collection: collection.into(),
                id: id.into(),
            },
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
        self.put_with(collection, id, doc, precondition, WriteOptions::default())
            .await
    }

    async fn put_with(
        &self,
        collection: &str,
        id: &str,
        doc: Value,
        precondition: Precondition,
        options: WriteOptions,
    ) -> Result<Version> {
        self.call_record(
            storage::PUT,
            collection,
            id,
            DocPutRequest {
                collection: collection.into(),
                id: id.into(),
                doc,
                precondition: serde_json::from_value(serde_json::to_value(precondition)?)?,
                schema: options.schema,
                expires_ms: options.expires_ms,
                created_ms: options.created_ms,
            },
        )
        .await
    }

    async fn delete(&self, collection: &str, id: &str, precondition: Precondition) -> Result<bool> {
        self.call_record(
            storage::DELETE,
            collection,
            id,
            DocDeleteRequest {
                collection: collection.into(),
                id: id.into(),
                precondition: serde_json::from_value(serde_json::to_value(precondition)?)?,
            },
        )
        .await
    }

    async fn query(&self, collection: &str, query: &Query) -> Result<Page<Versioned<Value>>> {
        self.call_set(
            storage::QUERY,
            collection,
            DocQueryRequest {
                collection: collection.into(),
                query: serde_json::from_value(serde_json::to_value(query)?)?,
            },
        )
        .await
    }

    async fn count(&self, collection: &str, filter: &Filter) -> Result<u64> {
        let _ = (collection, filter);
        Err(StorageError::backend(
            "count is outside the workflow catalog port",
        ))
    }

    async fn delete_where(&self, collection: &str, filter: &Filter) -> Result<u64> {
        self.call_set(
            storage::DELETE_WHERE,
            collection,
            DocDeleteWhereRequest {
                collection: collection.into(),
                filter: serde_json::from_value(serde_json::to_value(filter)?)?,
            },
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
            "claim is outside the catalog document port",
        ))
    }

    async fn drop_collection(&self, _collection: &str) -> Result<()> {
        Err(StorageError::backend(
            "drop_collection is outside the catalog document port",
        ))
    }
}

#[cfg(test)]
#[path = "catalog_documents_tests.rs"]
mod tests;
