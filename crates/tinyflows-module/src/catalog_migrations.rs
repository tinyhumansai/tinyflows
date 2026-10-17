//! Explicit, scope-bound catalog storage migration.

use anyhow::Result;
use std::sync::Arc;
use tinybus::Connection;
use tinyflows_bus::catalog::CatalogStorage;
use tinyflows_drivers::catalog::FlowCatalogDocuments;
use tinyflows_drivers::{catalog_storage_schema, document_only_scope, import_legacy_kv};
use tinystoragedrivers_core::{MigrateOptions, Registry, Scope};

pub(super) async fn migrate(connection: Connection, storage: CatalogStorage) -> Result<()> {
    let mut documents =
        super::catalog_documents::Documents::new(connection, storage.callback).await?;
    documents.require_record_metadata().await?;
    let _guard = super::migration_lock::catalog().await;
    let documents = Arc::new(documents);
    let scoped = document_only_scope(Scope::local(), documents.clone())?;
    FlowCatalogDocuments::new(documents).ensure().await?;
    let registry = Registry::compose([catalog_storage_schema()])?;
    let report = registry
        .migrate(&scoped, &MigrateOptions::default())
        .await?;
    let imported_kv = import_legacy_kv(&scoped).await?;
    tracing::debug!(
        steps_run = report.ran.len(),
        already_done = report.already_done,
        imported_kv,
        "catalog migration complete"
    );
    Ok(())
}
