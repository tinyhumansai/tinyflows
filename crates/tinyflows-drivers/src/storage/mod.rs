//! The storage schema v2 declarations of this crate.
//!
//! - [`storage_schema()`] is owner `flows` (scope class Tenant): the flow
//!   catalog (`flows.definitions`, `.revisions`, `.runs`, `.suggestions`,
//!   `.drafts`) and the library workflow store (feature `store`:
//!   `flows.workflows`, `.workflow_revisions`, `.workflow_runs`,
//!   `.workflow_notes`, `.workflow_proposals`, `.workflow_quarantine`). An
//!   owner may be declared once, so `tinyflows-adaptive` extends this same
//!   schema with `flows.adaptive_*`.
//! - [`cron_storage_schema()`] is owner `cron`: `cron.jobs`, `cron.runs`,
//!   `cron.admission`.
//! - The graph checkpointer is **not** declared here: owner `graph`
//!   (`tinyagents-graph`) declares `graph.{checkpoints,threads,writes}` and
//!   migrates `flows_graph_*` into namespace `flows`.
//! - Key-value state and counters live in `tsd.kv` / `tsd.counters`; a document
//!   step cannot write another owner's collections, so [`import_legacy_kv`] and
//!   [`import_legacy_cron_counters`] move them (host calls them once per scope
//!   after `Registry::migrate`).
//!
//! A host composes `Registry::compose([storage_schema(), cron_storage_schema(),
//! ...])` and runs `Registry::migrate` per scope.

mod kv_import;

use std::sync::Arc;

use tinystoragedrivers_core::{
    DocumentStore, MemoryStorage, Owner, Schema, Scope, ScopedStorage, StorageBackend,
};

pub use kv_import::{
    CATALOG_NAMESPACE_PREFIX, KV_OWNER, STATE_NAMESPACE, catalog_namespace, import_legacy_kv,
};

/// The owner name of the flows schema.
pub const FLOWS_OWNER: &str = "flows";

/// An empty schema for owner `flows`, for the feature-gated pieces to extend.
#[allow(clippy::expect_used, reason = "`flows` is a valid owner name")]
fn flows_schema() -> Schema {
    Schema::new(Owner::new(FLOWS_OWNER).expect("`flows` is a valid owner"))
}

/// The `flows` owner's declarations and v1 -> v2 steps for the features this
/// build has. `tinyflows-adaptive` extends the result with its collections
/// (`tinyflows_adaptive::drivers::extend_storage_schema`): an owner may be
/// declared only once, so the host passes one schema through both.
#[must_use]
pub fn storage_schema() -> Schema {
    #[allow(unused_mut)]
    let mut schema = flows_schema();
    #[cfg(feature = "catalog")]
    {
        schema = crate::catalog::extend_schema(schema);
    }
    #[cfg(feature = "store")]
    {
        schema = crate::store::extend_schema(schema);
    }
    schema
}

/// The catalog-only portion of owner `flows` and its v1 -> v2 steps.
///
/// Unlike [`storage_schema`], this excludes workflow-store schemas so the
/// catalog module can migrate only the collections behind its own callback.
#[cfg(feature = "catalog")]
#[must_use]
pub fn catalog_storage_schema() -> Schema {
    crate::catalog::extend_schema(flows_schema())
}

/// Assemble a scoped document-only migration view over a host callback.
///
/// The stream and blob handles are inert memory ports because the TinyFlows
/// catalog and cron migration schemas contain document steps only. They are
/// never read or written by these migrations.
pub fn document_only_scope(
    scope: Scope,
    documents: Arc<dyn DocumentStore>,
) -> tinystoragedrivers_core::Result<ScopedStorage> {
    let memory = MemoryStorage::new().for_scope(&scope)?;
    Ok(ScopedStorage::new(
        scope,
        "tinyflows-document-callback",
        documents,
        memory.streams().clone(),
        memory.blobs().clone(),
    ))
}

/// The `cron` owner's declarations and v1 -> v2 steps.
#[cfg(feature = "schedule")]
#[must_use]
pub fn cron_storage_schema() -> Schema {
    crate::schedule::storage_schema()
}

#[cfg(feature = "schedule")]
pub use crate::schedule::import_legacy_cron_counters;
