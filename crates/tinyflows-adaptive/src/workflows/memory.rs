//! A vault that forgets, for tests and for a first look.
//!
//! Same posture as [`crate::ledger::memory`]: always compiled, never the
//! default, named for what it does.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tinyflows::store::types::{WorkflowError, WorkflowRecord};

use super::Vault;

/// A vault held in memory, which keeps nothing across restarts.
#[derive(Clone, Default)]
pub struct MemoryVault {
    /// `(bucket, id)` so scoping behaves exactly as the durable backends do.
    inner: Arc<Mutex<BTreeMap<(String, String), WorkflowRecord>>>,
    scope: Option<String>,
}

impl MemoryVault {
    /// An empty vault.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A handle onto the same store, scoped to one tenant.
    #[must_use]
    pub fn for_tenant(&self, scope: impl Into<String>) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            scope: Some(scope.into()),
        }
    }

    fn bucket(&self) -> String {
        self.scope.clone().unwrap_or_default()
    }
}

#[async_trait]
impl Vault for MemoryVault {
    fn scope(&self) -> Option<&str> {
        self.scope.as_deref()
    }

    async fn load(&self) -> Result<Vec<WorkflowRecord>, WorkflowError> {
        let bucket = self.bucket();
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // This bucket plus global, one record per id, and the bucket's own
        // record shadows a global one — explicitly, not as an accident of map
        // iteration order.
        let mut chosen: std::collections::BTreeMap<String, WorkflowRecord> =
            std::collections::BTreeMap::new();
        for ((scope, id), record) in inner.iter() {
            if scope.is_empty() {
                chosen.insert(id.clone(), record.clone());
            }
        }
        for ((scope, id), record) in inner.iter() {
            if !bucket.is_empty() && scope == &bucket {
                chosen.insert(id.clone(), record.clone());
            }
        }
        Ok(chosen.into_values().collect())
    }

    async fn put(&self, record: &WorkflowRecord) -> Result<(), WorkflowError> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert((self.bucket(), record.id.clone()), record.clone());
        Ok(())
    }

    async fn remove(&self, id: &str) -> Result<(), WorkflowError> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&(self.bucket(), id.to_string()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflows::conformance;

    #[tokio::test]
    async fn passes_the_conformance_suite() {
        conformance::run_all(&MemoryVault::new()).await;
    }

    #[tokio::test]
    async fn passes_the_tenant_isolation_suite() {
        let vault = MemoryVault::new();
        conformance::run_tenants(&vault, &vault.for_tenant("a"), &vault.for_tenant("b")).await;
    }
}
