//! `tinyflows` run state and graph checkpoints on
//! [tinystoragedrivers](https://github.com/tinyhumansai/tinystoragedrivers)
//! document ports.
//!
//! A host that has opened a storage backend (SQLite on a desktop, MongoDB in
//! the cloud, memory in tests) hands these a document handle — usually one
//! already bound to a tenant scope — and plugs them into the engine:
//!
//! - `DriverStateStore` is the engine's [`StateStore`] capability
//!   (`Capabilities::state`), one document per key.
//! - `DriverCheckpointer` is a graph [`Checkpointer`]: checkpoints, pending
//!   writes, namespace-scoped reads and a bulk `state_history`, for
//!   `engine::run_with_checkpointer` / `resume_with_checkpointer`.
//!
//! - `catalog` (feature `catalog`) is the flow catalog and authoring
//!   drafts: `catalog::FlowCatalogDocuments` (flows, revisions, runs,
//!   suggestions, drafts), and `catalog::FlowStateDocuments`, one flow's
//!   namespaced state.
//! - `store::DriverWorkflowStore` (feature `store`) is the engine's
//!   `WorkflowStore` — saved workflows with revisions, run records, the notes
//!   journal and proposals — on documents.
//! - `schedule::CronDocuments` (feature `schedule`) is the cron job and
//!   run-history store.
//!   It needs only `tinyflows-schedule`, not the engine.
//!
//! Everything stores on the uniform storage schema v2: [`storage_schema()`]
//! (owner `flows`) and [`cron_storage_schema()`] (owner `cron`) declare the
//! collections and carry the v1 -> v2 steps, and [`import_legacy_kv`] moves the
//! retired key-value collections into `tsd.kv`. The graph checkpointer's
//! `graph.*` collections are declared and migrated by `tinyagents-graph`
//! (namespace `flows`), not here.
//!
//! The items above are behind Cargo features, so they are named rather than
//! linked: a link would not resolve in a build without its feature.
//!
//! [`StateStore`]: https://docs.rs/tinyflows/latest/tinyflows/caps/trait.StateStore.html
//! [`Checkpointer`]: https://docs.rs/tinyflows/latest/tinyflows/graph/checkpoint/trait.Checkpointer.html

#[cfg(feature = "catalog")]
pub mod catalog;
#[cfg(feature = "engine")]
mod checkpoint;
#[cfg(feature = "engine")]
mod checkpoint_layout;
#[cfg(any(feature = "catalog", feature = "schedule"))]
mod ids;
#[cfg(feature = "schedule")]
pub mod schedule;
#[cfg(feature = "engine")]
mod state_store;
pub mod storage;
#[cfg(feature = "store")]
pub mod store;

#[cfg(feature = "engine")]
pub use checkpoint::DriverCheckpointer;
#[cfg(feature = "engine")]
pub use checkpoint_layout::NAMESPACE;
#[cfg(feature = "engine")]
pub use state_store::DriverStateStore;

#[cfg(feature = "catalog")]
pub use storage::catalog_storage_schema;
pub use storage::{KV_OWNER, document_only_scope, import_legacy_kv, storage_schema};
#[cfg(feature = "schedule")]
pub use storage::{cron_storage_schema, import_legacy_cron_counters};
