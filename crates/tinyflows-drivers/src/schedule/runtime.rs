//! Apply computed scheduler mutations with version-checked document writes.
use super::{
    CAS_ATTEMPTS, CronDocuments, JOBS,
    codec::{doc_to_job, set_last_run},
    compare_job, storage_error,
};
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::Value;
use tinyflows_schedule::{CronJob, Schedule, runtime::Completion, truncate_cron_output};
use tinystoragedrivers_core::{Version, Versioned};
fn decode(id: &str, doc: &Value) -> Result<CronJob> {
    doc_to_job(&Versioned {
        id: id.into(),
        version: Version::FIRST,
        doc: doc.clone(),
    })
}
impl CronDocuments {
    /// Advance only the original occurrence and definition incarnation.
    /// Disable one-shots durably before execution to prevent replay after a crash.
    pub async fn claim_slot(&self, job: &CronJob, next: DateTime<Utc>) -> Result<bool> {
        self.ensure().await?;
        Ok(compare_job(self, JOBS, &job.id, |doc| {
            let current = decode(&job.id, doc)?;
            if !current.enabled
                || current.created_at != job.created_at
                || !current.same_execution_definition(job)
                || current.schedule != job.schedule
                || current.next_run != job.next_run
            {
                return Ok(None);
            }
            let mut next_doc = doc.as_object().cloned().unwrap_or_default();
            super::jobs::reschedule(
                &mut next_doc,
                next,
                matches!(job.schedule, Schedule::At { .. }),
            );
            Ok(Some(Value::Object(next_doc)))
        })
        .await?
        .is_some())
    }
    /// Record the outcome while preserving concurrent schedule/enable edits and
    /// newer next occurrences. Call only after claim_slot disabled one-shots;
    /// enabled one-shots now represent an explicit re-enable. Deletes use the
    /// observed document version and preserve such re-enables.
    pub async fn complete_run(&self, job: &CronJob, completion: &Completion) -> Result<()> {
        self.ensure().await?;
        if completion.remove {
            for attempt in 0..CAS_ATTEMPTS {
                let Some(stored) = self.docs.get(JOBS, &job.id).await.map_err(storage_error)?
                else {
                    return Ok(());
                };
                let current = doc_to_job(&stored)?;
                if current.created_at != job.created_at {
                    return Ok(());
                }
                if current.enabled
                    || !current.delete_after_run
                    || current.schedule != job.schedule
                    || current.command != job.command
                    || current.prompt != job.prompt
                {
                    break;
                }
                if self.remove_unchanged(&stored).await? {
                    return Ok(());
                }
                anyhow::ensure!(
                    attempt + 1 < CAS_ATTEMPTS,
                    "completion deletion kept changing under {CAS_ATTEMPTS} attempts"
                );
            }
        }
        compare_job(self, JOBS, &job.id, |doc| {
            let current = decode(&job.id, doc)?;
            if current.created_at != job.created_at {
                return Ok(None);
            }
            let same = current.schedule == job.schedule;
            let mut next = doc.as_object().cloned().unwrap_or_default();
            set_last_run(
                &mut next,
                completion.finished_at,
                completion.success,
                truncate_cron_output(&completion.output),
            );
            if let Some(time) = completion
                .next_run
                .filter(|time| same && *time >= current.next_run)
            {
                super::jobs::reschedule(&mut next, time, false);
            }
            // One-shots were disabled at claim time; enabled now means
            // an explicit re-enable that this old run must preserve.
            if same && completion.disable && !matches!(job.schedule, Schedule::At { .. }) {
                next.insert("enabled".into(), Value::Bool(false));
            }
            Ok(Some(Value::Object(next)))
        })
        .await?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
