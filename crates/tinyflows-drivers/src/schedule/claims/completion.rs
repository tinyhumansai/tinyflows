//! Fenced terminal history and schedule mutation for occurrence leases.
use super::super::codec::{doc_to_job, run_id, run_to_doc, set_last_run};
use super::super::jobs::reschedule;
use super::super::{CAS_ATTEMPTS, CronDocuments, OWNER, RUNS, storage_error};
use super::{
    CLAIM_EPOCH, CLAIM_OWNER, COMPLETED, ClaimedOccurrence, JOBS, QUEUED, Receipt, lease_key,
    queued_run_cleanup,
};
use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use tinyflows_schedule::{DeliveryStatus, Schedule, next_run_for_schedule, truncate_cron_output};
use tinystoragedrivers_core::{Capability, Precondition, WriteOp};

impl CronDocuments {
    /// Atomically records a run and advances its still-owned occurrence.
    /// Stale, expired, edited, canceled or re-created jobs return false
    /// without any outcome write. A one-shot is disabled, retaining history.
    /// The host applies delete_after_run with [`Self::remove_completed_claim`]
    /// separately when it wants removal. Never follow completion with
    /// `remove_job(job_id)`: that id may already belong to a replacement.
    /// The claim's queued placeholder run, if any, is replaced by the
    /// terminal run.
    ///
    /// # Errors
    /// Completion predates acquisition, state/schedule is malformed, transactions
    /// are unavailable, retries are exhausted, or the backend fails. Once the
    /// run and advance have committed the call is `Ok(true)`: a failed lease
    /// release or history prune is logged, never returned.
    pub async fn complete_claim(
        &self,
        claim: &ClaimedOccurrence,
        finished_at: DateTime<Utc>,
        success: bool,
        output: &str,
        delivery_status: Option<DeliveryStatus>,
    ) -> Result<bool> {
        self.complete_claim_mode(claim, finished_at, success, output, delivery_status, true)
            .await
    }

    /// Atomically records a manual run without advancing or mutating its saved
    /// schedule, enabled state, or delete-after-run policy.
    ///
    /// # Errors
    /// As [`Self::complete_claim`].
    pub async fn complete_manual_claim(
        &self,
        claim: &ClaimedOccurrence,
        finished_at: DateTime<Utc>,
        success: bool,
        output: &str,
        delivery_status: Option<DeliveryStatus>,
    ) -> Result<bool> {
        self.complete_claim_mode(claim, finished_at, success, output, delivery_status, false)
            .await
    }

    async fn complete_claim_mode(
        &self,
        claim: &ClaimedOccurrence,
        finished_at: DateTime<Utc>,
        success: bool,
        output: &str,
        delivery_status: Option<DeliveryStatus>,
        advance_schedule: bool,
    ) -> Result<bool> {
        self.docs
            .capabilities()
            .require(Capability::Transactions)
            .map_err(storage_error)?;
        self.ensure().await?;
        if finished_at < claim.lease.started_at {
            bail!("cron store: completion precedes acquisition");
        }
        for _ in 0..CAS_ATTEMPTS {
            let Some(stored) = self
                .docs
                .get(JOBS, &claim.job.id)
                .await
                .map_err(storage_error)?
            else {
                return Ok(false);
            };
            if !self.owns(&stored, claim, finished_at).await? {
                return Ok(false);
            }
            let job = doc_to_job(&stored)?;
            let mut doc = stored.doc.clone();
            let body = doc
                .as_object_mut()
                .ok_or_else(|| anyhow::anyhow!("cron store: invalid job"))?;
            body.remove(QUEUED);
            body.remove(CLAIM_EPOCH);
            body.remove(CLAIM_OWNER);
            body.insert(
                COMPLETED.into(),
                serde_json::to_value(Receipt {
                    owner: claim.lease.owner.clone(),
                    token: claim.lease.token.clone(),
                    epoch: claim.lease.epoch,
                    incarnation: claim.lease.incarnation.clone(),
                    occurrence_ms: claim.lease.occurrence.timestamp_millis(),
                })?,
            );
            set_last_run(body, finished_at, success, truncate_cron_output(output));
            if advance_schedule {
                reschedule(
                    body,
                    next_run_for_schedule(&job.schedule, finished_at)?,
                    matches!(job.schedule, Schedule::At { .. }),
                );
            }
            let seq = self.next_run_number().await?;
            let run = run_to_doc(
                seq,
                &job.id,
                Some(&claim.lease.incarnation),
                claim.lease.started_at,
                finished_at,
                if success { "ok" } else { "error" },
                Some(truncate_cron_output(output)),
                (finished_at - claim.lease.started_at).num_milliseconds(),
                delivery_status.as_ref(),
            );
            let mut extra = vec![WriteOp::Put {
                collection: RUNS.into(),
                id: run_id(seq),
                doc: run,
                precondition: Precondition::Absent,
            }];
            extra.extend(queued_run_cleanup(&stored.doc)?);
            match self
                .commit_job(&job.id, Some(&stored), Some(doc), extra)
                .await
            {
                Ok(_) => {
                    // The run, receipt and advance are durable, so nothing
                    // below may fail the completion: a caller that saw an
                    // error would skip `delete_after_run` removal and report
                    // a finished run as failed. Ending the lease and pruning
                    // history are liveness only (`claim_epoch` already fences
                    // a stale holder and the lease expires on its own).
                    match lease_key(&job.id, claim.lease.occurrence.timestamp_millis()) {
                        Ok(key) => {
                            if let Err(error) = self
                                .leases_at(finished_at)
                                .release(OWNER, &key, &claim.lease.token, claim.lease.epoch)
                                .await
                            {
                                tracing::warn!(
                                    job_id = %job.id,
                                    epoch = claim.lease.epoch,
                                    %error,
                                    "[cron-claims] releasing a completed claim's lease failed; it expires on its own"
                                );
                            }
                        }
                        Err(error) => tracing::warn!(
                            job_id = %job.id,
                            epoch = claim.lease.epoch,
                            %error,
                            "[cron-claims] no lease key for a completed claim"
                        ),
                    }
                    if let Err(error) = self.prune_runs(&job.id, &stored.doc).await {
                        tracing::warn!(
                            job_id = %job.id,
                            %error,
                            "[cron-claims] pruning run history after completion failed"
                        );
                    }
                    return Ok(true);
                }
                Err(error) if super::super::admission::conflict(&error) => {}
                Err(error) => return Err(error),
            }
        }
        bail!("cron store: occurrence kept changing during completion")
    }
}
