//! Loadable workflow authoring module. Hosts depend on `tinyflows-bus` and
//! invoke the compiled module; the engine stays behind that boundary.

mod copilot_tool_belt;
mod copilot_tools;
mod host;
mod service;
mod validation;

pub mod caps;
mod catalog_authoring;
mod catalog_documents;
mod catalog_extended_ops;
mod catalog_migrations;
mod catalog_service;
mod catalog_service_summary;
mod exports;
mod migration_lock;
mod schedule_callbacks;
mod schedule_service;

mod portability_service;
mod run_manager;
mod runs_service;
mod schedule_documents;
mod schedule_store;

#[cfg(feature = "static-link")]
pub use exports::linked_module;
pub use tinyflows_bus::{BUS_NAME, OBJECT_PATH};
