//! Upgrade legacy host policies through CAS; never lose fields or delete first.
use super::*;
use tinystoragedrivers_core::{DocumentStoreExt, ErrorKind, Query};

impl CronDocuments {
    pub(super) async fn migrate_policies(&self) -> Result<()> {
        const LEGACY: &str = "cron_job_policies";
        self.docs
            .ensure_collection(&CollectionSpec::new(LEGACY))
            .await
            .map_err(storage_error)?;
        for policy in self
            .docs
            .query_all(LEGACY, &Query::all())
            .await
            .map_err(storage_error)?
        {
            // Missing job is an orphan; the old table also permitted such rows.
            // A pre-existing policy means an interrupted migration already wrote
            // it, or a new writer has edited it. Never replace that newer value.
            compare_job(self, JOBS, &policy.id, |doc| {
                if doc.get("policy").is_some() {
                    return Ok(None);
                }
                let mut doc = doc.clone();
                let retries = policy
                    .doc
                    .get("retries")
                    .and_then(Value::as_u64)
                    .map(|n| u32::try_from(n).unwrap_or(u32::MAX));
                let single_flight = policy
                    .doc
                    .get("single_flight")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                doc["policy"] = serde_json::to_value(tinyflows_schedule::JobPolicy {
                    retries,
                    single_flight,
                })?;
                Ok(Some(doc))
            })
            .await?;
            // Delete only the exact policy version we copied; retry after a
            // concurrent legacy edit rather than discarding that edit.
            match self
                .docs
                .delete(LEGACY, &policy.id, policy.unchanged())
                .await
            {
                Ok(_) => {}
                Err(error) if error.kind() == ErrorKind::Conflict => {}
                Err(error) => return Err(storage_error(error)),
            }
        }
        Ok(())
    }
}
