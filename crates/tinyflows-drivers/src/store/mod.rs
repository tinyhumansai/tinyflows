//! `WorkflowStore` on a `tinystoragedrivers` document handle.
//!
//! [`DriverWorkflowStore`] is the storage behind the engine's authoring layer
//! (`tinyflows::store`): saved workflow definitions, their superseded
//! revisions, run records, the per-workflow notes journal and proposed graph
//! changes. A host that opened a storage backend (SQLite on a desktop, MongoDB
//! in the cloud, memory in tests) hands it a document handle, usually one bound
//! to a tenant scope.
//!
//! # Layout (storage schema v2, owner `flows`)
//!
//! | Collection | Document id | Holds |
//! | --- | --- | --- |
//! | `flows.workflows` | `Key::of([workflow id])` | one `WorkflowRecord` |
//! | `flows.workflow_revisions` | revision id | one superseded copy of a workflow |
//! | `flows.workflow_runs` | `Key::of([run id])` | one `RunRecord` |
//! | `flows.workflow_notes` | `Key::of([workflow id])` | the workflow's whole journal (at most [`MAX_NOTES`]) |
//! | `flows.workflow_proposals` | `Key::of([proposal id])` | one `WorkflowProposal` |
//! | `flows.workflow_quarantine` | ULID | a journal that could not be parsed, set aside |
//!
//! The names carry `workflow_` because the records are not the catalog's: a
//! `WorkflowRecord` is not a `Flow` (`flows.definitions`), nor a `RunRecord`
//! a catalog run (`flows.runs`). The decision lock is not a collection of this
//! owner: it is a `tsd.leases` lease (owner `flows`, key
//! `workflow_decision/<workflow id>`).
//!
//! Bodies are bare snake_case objects: a graph, a run's steps, a journal's
//! notes and a proposal's ops are real JSON values, not strings, and every
//! instant is one epoch-millisecond `*_ms` field (`started_ms`, `finished_ms`,
//! `recorded_ms`, `created_ms`, `decided_ms`, `superseded_ms`). The model types
//! keep their camelCase wire form; the conversion is in `codec`. Ids longer
//! than a key allows are hashed by `Key::of`; the record itself keeps the real
//! id in its `id` field.
//!
//! `flows_*`-style v1 data (`wf_*`, JSON strings under `record_json`,
//! `run_json`, `notes_json`, `proposal_json`) moves with the steps in
//! `storage_schema()`; see `migrate`.
//!
//! # What the port gives in place of files
//!
//! The file store serialised writers with advisory file locks. The port has
//! single-document compare-and-swap instead, so:
//!
//! - every definition write is a compare-and-swap on the version it read. A
//!   save or delete first writes the revision of what it is about to replace,
//!   then swaps the definition, and takes the revision back when the swap loses
//!   (then retries). A crash between the two leaves one extra revision, never
//!   a lost one;
//! - `WorkflowStore::save_if_fingerprint` and
//!   `WorkflowStore::save_if_record_fingerprint` compare the fingerprint on
//!   the document they swap, so two processes (or store instances) racing the
//!   same edit cannot both win;
//! - the journal is one document per workflow, updated by compare-and-swap;
//! - `WorkflowStore::lock_proposal_decision` is a `tsd.leases` lease, so it
//!   holds across processes sharing one database. The lease lasts
//!   [`LOCK_LEASE`]; a decision that takes longer than that loses its
//!   exclusion.
//!
//! The trait is synchronous, so each call runs on `tinystoragedrivers`'
//! [`Blocking`] bridge (one per process, or the host's own through
//! [`DriverWorkflowStore::with_bridge`]), which is safe from inside any runtime.

use std::sync::{Arc, OnceLock};

use tinyflows::store::types::WorkflowError;
use tinyflows::store::{EnginePolicy, HostPolicy, MAX_NOTES};
use tinyflows_storage::{
    Blocking, CollectionSpec, DocumentStore, IndexSpec, Key, Precondition, StorageError, Version,
    WriteOptions, primitives,
};
use tokio::sync::OnceCell;

mod codec;
mod definitions;
mod history;
mod import;
mod locks;
mod migrate;
mod notes;
mod proposals;
mod runs;
mod trait_impl;

pub use locks::LOCK_LEASE;
pub(crate) use migrate::extend_schema;

pub(crate) const DEFINITIONS: &str = "flows.workflows";
pub(crate) const REVISIONS: &str = "flows.workflow_revisions";
pub(crate) const RUNS: &str = "flows.workflow_runs";
pub(crate) const NOTES: &str = "flows.workflow_notes";
pub(crate) const PROPOSALS: &str = "flows.workflow_proposals";
pub(crate) const QUARANTINE: &str = "flows.workflow_quarantine";

/// The body version of every collection here (`wf_*` was version 1).
pub(crate) const SCHEMA_VERSION: u32 = 2;

/// Compare-and-swap attempts before a contended write gives up.
pub(crate) const CAS_ATTEMPTS: usize = 32;

/// What the async halves share.
pub(crate) struct Inner {
    pub(crate) docs: Arc<dyn DocumentStore>,
    pub(crate) policy: Arc<dyn HostPolicy>,
    declared: Arc<OnceCell<()>>,
}

/// The workflow store on a document handle. Cheap to clone; clones share the
/// handle, the policy and the bridge.
#[derive(Clone)]
pub struct DriverWorkflowStore {
    inner: Arc<Inner>,
    bridge: Option<Arc<Blocking>>,
    scope_label: String,
}

impl std::fmt::Debug for DriverWorkflowStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DriverWorkflowStore")
            .field("scope_label", &self.scope_label)
            .finish_non_exhaustive()
    }
}

impl DriverWorkflowStore {
    /// The store in `docs`, judging documents by the engine's own rules.
    pub fn new(docs: Arc<dyn DocumentStore>) -> Self {
        Self {
            inner: Arc::new(Inner {
                docs,
                policy: Arc::new(EnginePolicy),
                declared: Arc::new(OnceCell::new()),
            }),
            bridge: None,
            scope_label: "documents".to_string(),
        }
    }

    /// The same store, judging every document and every edit by `policy`.
    ///
    /// Applied to reads as well as writes, so a document that became invalid
    /// because the host's vocabulary changed under it is reported the next time
    /// it is read rather than silently running somewhere else.
    #[must_use]
    pub fn with_policy(self, policy: Arc<dyn HostPolicy>) -> Self {
        Self {
            inner: Arc::new(Inner {
                docs: Arc::clone(&self.inner.docs),
                policy,
                declared: Arc::clone(&self.inner.declared),
            }),
            ..self
        }
    }

    /// The same store, blocking on `bridge` rather than the process-wide one.
    #[must_use]
    pub fn with_bridge(self, bridge: Arc<Blocking>) -> Self {
        Self {
            bridge: Some(bridge),
            ..self
        }
    }

    /// The same store, naming itself `label` in
    /// `WorkflowStore::proposal_decision_scope`. Instances over the same
    /// storage should share a label; unrelated catalogs in one process should
    /// not.
    #[must_use]
    pub fn with_scope_label(self, label: impl Into<String>) -> Self {
        Self {
            scope_label: label.into(),
            ..self
        }
    }

    /// Every collection the store uses, with its indexes.
    pub fn collections() -> Vec<CollectionSpec> {
        vec![
            CollectionSpec::new(DEFINITIONS),
            CollectionSpec::new(REVISIONS).index(IndexSpec::new(
                "by_workflow",
                ["workflow_id", "superseded_ms"],
            )),
            CollectionSpec::new(RUNS)
                .index(IndexSpec::new("by_workflow", ["workflow_id", "started_ms"]))
                .index(IndexSpec::new("by_settled", ["settled", "started_ms"])),
            CollectionSpec::new(NOTES),
            CollectionSpec::new(PROPOSALS)
                .index(IndexSpec::new("by_workflow", ["workflow_id", "created_ms"])),
            CollectionSpec::new(QUARANTINE),
        ]
    }

    /// Declares [`Self::collections`]. Every method does this on first use; a
    /// host may call it at startup to fail early.
    ///
    /// # Errors
    ///
    /// When the backend refuses a collection or index.
    pub fn ensure(&self) -> Result<(), WorkflowError> {
        let inner = Arc::clone(&self.inner);
        self.block(async move { inner.ensure().await })
    }

    /// Runs `future` to completion on the bridge.
    pub(crate) fn block<T, F>(&self, future: F) -> Result<T, WorkflowError>
    where
        F: std::future::Future<Output = Result<T, WorkflowError>> + Send + 'static,
        T: Send + 'static,
    {
        let ran = match &self.bridge {
            Some(bridge) => bridge.run(future),
            None => shared_bridge()?.run(future),
        };
        ran.map_err(storage)?
    }
}

fn shared_bridge() -> Result<&'static Blocking, WorkflowError> {
    static BRIDGE: OnceLock<Result<Blocking, String>> = OnceLock::new();
    BRIDGE
        .get_or_init(|| Blocking::new().map_err(|error| error.to_string()))
        .as_ref()
        .map_err(|error| WorkflowError::Storage(error.clone()))
}

impl Inner {
    pub(crate) async fn ensure(&self) -> Result<(), WorkflowError> {
        self.declared
            .get_or_try_init(|| async {
                for spec in DriverWorkflowStore::collections()
                    .into_iter()
                    .chain(primitives::collection_specs())
                {
                    self.docs.ensure_collection(&spec).await.map_err(storage)?;
                }
                Ok::<(), WorkflowError>(())
            })
            .await
            .map(|_| ())
    }
}

/// A storage failure as the store's error.
pub(crate) fn storage(error: StorageError) -> WorkflowError {
    WorkflowError::Storage(error.to_string())
}

/// A (de)serialization failure as the store's error.
pub(crate) fn malformed(context: &str, error: impl std::fmt::Display) -> WorkflowError {
    WorkflowError::Malformed(format!("{context}: {error}"))
}

/// The document id for an id the caller chose (`Key::of`, which hashes one
/// too long to use as a key): refused when empty or containing a control
/// character.
pub(crate) fn doc_id(id: &str) -> Result<String, WorkflowError> {
    if id.is_empty() || id.chars().any(char::is_control) {
        return Err(WorkflowError::Malformed(format!(
            "{id:?} is not a usable id: it is empty or contains a control character"
        )));
    }
    Key::of([id])
        .map(|key| key.as_str().to_owned())
        .map_err(storage)
}

/// Writes `doc` at `collection/id` under `precondition`, declaring the
/// stores' body version.
pub(crate) async fn put_doc(
    docs: &dyn DocumentStore,
    collection: &str,
    id: &str,
    doc: serde_json::Value,
    precondition: Precondition,
) -> Result<Version, StorageError> {
    docs.put_with(
        collection,
        id,
        doc,
        precondition,
        WriteOptions::new().schema(SCHEMA_VERSION),
    )
    .await
}

/// Epoch milliseconds, saturating at zero if the clock is before the epoch.
pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

/// A stamp as the signed integer documents order and filter on.
pub(crate) fn stamp(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// How many notes a journal keeps; re-exported so a host can size around it.
pub const JOURNAL_CAP: usize = MAX_NOTES;

#[cfg(test)]
pub(crate) mod test_support;

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "authoring_tests.rs"]
mod authoring_tests;

#[cfg(test)]
#[path = "concurrency_tests.rs"]
mod concurrency_tests;

#[cfg(test)]
#[path = "history_tests.rs"]
mod history_tests;

#[cfg(test)]
#[path = "locks_tests.rs"]
mod locks_tests;

#[cfg(test)]
#[path = "migrate_tests.rs"]
mod migrate_tests;

#[cfg(test)]
#[path = "notes_tests.rs"]
mod notes_tests;

#[cfg(test)]
#[path = "proposals_tests.rs"]
mod proposals_tests;

#[cfg(test)]
#[path = "runs_tests.rs"]
mod runs_tests;
