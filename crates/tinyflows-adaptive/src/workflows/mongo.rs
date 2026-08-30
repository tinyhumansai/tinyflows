//! Workflows in the same MongoDB database as the ledger.

use async_trait::async_trait;
use mongodb::bson::{Document, doc};
use mongodb::{Client, Collection, Database};
use tinyflows::store::types::{WorkflowError, WorkflowRecord};

use super::Vault;

const WORKFLOWS: &str = "workflows";

/// A vault backed by a MongoDB database.
#[derive(Clone)]
pub struct MongoVault {
    db: Database,
    scope: Option<String>,
}

impl MongoVault {
    /// Connect to `uri` and use the database named `database`.
    ///
    /// # Errors
    /// When the URI is malformed or the server is unreachable.
    pub async fn connect(uri: &str, database: &str) -> Result<Self, WorkflowError> {
        let client = Client::with_uri_str(uri)
            .await
            .map_err(|e| WorkflowError::Engine(e.to_string()))?;
        Self::with_database(client.database(database)).await
    }

    /// Use an already-connected database, for a host managing its own pool.
    ///
    /// Async because it creates the unique `(scope_key, workflow_id)` index —
    /// without it, two replicas upserting the same workflow at once can insert
    /// duplicate documents, and `load` would return whichever the cursor met
    /// first.
    ///
    /// # Errors
    /// When the index cannot be created.
    pub async fn with_database(db: Database) -> Result<Self, WorkflowError> {
        let vault = Self { db, scope: None };
        vault.ensure_indexes().await?;
        Ok(vault)
    }

    async fn ensure_indexes(&self) -> Result<(), WorkflowError> {
        let unique = mongodb::options::IndexOptions::builder()
            .unique(true)
            .build();
        self.workflows()
            .create_index(
                mongodb::IndexModel::builder()
                    .keys(doc! { "scope_key": 1, "workflow_id": 1 })
                    .options(unique)
                    .build(),
            )
            .await
            .map_err(mongo)?;
        Ok(())
    }

    /// A handle onto the same database, scoped to one tenant.
    #[must_use]
    pub fn for_tenant(&self, scope: impl Into<String>) -> Self {
        Self {
            db: self.db.clone(),
            scope: Some(scope.into()),
        }
    }

    /// Stored as a present empty string rather than an absent field, so the
    /// upsert filter matches one document — the same reason the ledger does it.
    fn bucket(&self) -> &str {
        self.scope.as_deref().unwrap_or_default()
    }

    fn workflows(&self) -> Collection<Document> {
        self.db.collection(WORKFLOWS)
    }
}

fn mongo(err: mongodb::error::Error) -> WorkflowError {
    WorkflowError::Engine(err.to_string())
}

#[async_trait]
impl Vault for MongoVault {
    fn scope(&self) -> Option<&str> {
        self.scope.as_deref()
    }

    async fn load(&self) -> Result<Vec<WorkflowRecord>, WorkflowError> {
        // This bucket plus global. A record written before scoping existed has
        // no field at all, which `$in` with "" does not match — but nothing
        // wrote one, because this collection is new.
        // Global first, then this bucket, so the bucket's own record shadows a
        // global one with the same id — precedence by construction, not by
        // whatever order the cursor happens to walk.
        let mut cursor = self
            .workflows()
            .find(doc! { "scope_key": { "$in": [self.bucket(), ""] } })
            .sort(doc! { "scope_key": 1, "workflow_id": 1 })
            .await
            .map_err(mongo)?;

        let mut chosen: std::collections::BTreeMap<String, WorkflowRecord> =
            std::collections::BTreeMap::new();
        while cursor.advance().await.map_err(mongo)? {
            let document = cursor.deserialize_current().map_err(mongo)?;
            let raw = document.get_str("document").unwrap_or_default();
            let record: WorkflowRecord = serde_json::from_str(raw).map_err(|e| {
                WorkflowError::Engine(format!("stored workflow no longer parses: {e}"))
            })?;
            chosen.insert(record.id.clone(), record);
        }
        Ok(chosen.into_values().collect())
    }

    async fn put(&self, record: &WorkflowRecord) -> Result<(), WorkflowError> {
        let document = serde_json::to_string(record)
            .map_err(|e| WorkflowError::Engine(format!("workflow will not serialize: {e}")))?;
        // Stored as a JSON string rather than a BSON subdocument: a node config
        // is arbitrary JSON, and BSON refuses keys containing a dot — which a
        // config keyed by a filename or a version has.
        //
        // One retry on a duplicate-key race: the unique index stops two
        // concurrent upserts both inserting, but the loser errors rather than
        // updating — its second pass finds the document and updates it.
        for attempt in 0..2 {
            let outcome = self
                .workflows()
                .update_one(
                    doc! { "scope_key": self.bucket(), "workflow_id": &record.id },
                    doc! { "$set": { "document": &document } },
                )
                .upsert(true)
                .await;
            match outcome {
                Ok(_) => return Ok(()),
                Err(e) if attempt == 0 && e.to_string().contains("E11000") => continue,
                Err(e) => return Err(mongo(e)),
            }
        }
        unreachable!("the loop returns on every branch of its final pass")
    }

    async fn remove(&self, id: &str) -> Result<(), WorkflowError> {
        self.workflows()
            .delete_one(doc! { "scope_key": self.bucket(), "workflow_id": id })
            .await
            .map_err(mongo)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflows::conformance;

    /// Needs a real server, so it is `#[ignore]` and visible in the run summary
    /// rather than silently skipped — the same posture as the mongo ledger.
    #[tokio::test]
    #[ignore = "needs a MongoDB server; set ADAPTIVE_MONGO_URI"]
    async fn passes_the_conformance_suite() {
        let uri = std::env::var("ADAPTIVE_MONGO_URI").expect("ADAPTIVE_MONGO_URI");
        let name = format!("adaptive_vault_{}", std::process::id());
        let vault = MongoVault::connect(&uri, &name).await.expect("connect");
        conformance::run_all(&vault).await;
        conformance::run_tenants(&vault, &vault.for_tenant("a"), &vault.for_tenant("b")).await;
        vault.db.drop().await.expect("drop the throwaway database");
    }
}
