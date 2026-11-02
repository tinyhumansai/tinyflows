//! Upgrade legacy host policies through CAS; never lose fields or delete first.
use super::*;
use tinyflows_storage::{DocumentStoreExt, ErrorKind, Query};

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
            compare_job(self, JOBS, &policy.id, |stored| {
                let doc = &stored.doc;
                if doc.get("policy").is_some() {
                    return Ok(None);
                }
                let mut doc = doc.clone();
                let retries = match policy.doc.get("retries") {
                    None | Some(Value::Null) => None,
                    Some(value) => Some(
                        value
                            .as_u64()
                            .and_then(|n| u32::try_from(n).ok())
                            .ok_or_else(|| anyhow!("Invalid legacy cron policy retries"))?,
                    ),
                };
                let single_flight = policy
                    .doc
                    .get("single_flight")
                    .and_then(Value::as_bool)
                    .ok_or_else(|| anyhow!("Invalid legacy cron policy single_flight"))?;
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
