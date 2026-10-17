//! Atomic occurrence leases and fenced run completion.
//!
//! A claim is a `tsd.leases` record (owner `cron`, key
//! `Key::of([job_id, occurrence_ms])`) held by a random token, so one occurrence
//! has one live holder across handles sharing the scope. The lease's epoch
//! moves on every new tenure, which is what fences a stalled worker. The job
//! document carries the active claim epoch and logical owner (separate from
//! the lease token) plus the receipt of the last completed claim
//! (`completed_occurrence`), which `remove_completed_claim` checks. While a
//! claim holds a durable "queued" placeholder run (`queue_claimed_run`), its
//! number is the job's `queued_run`; every end of the claim (completion,
//! release, a new acquisition, an edit) removes the placeholder.
//!
//! The lease record and the job document are two writes, so the job carries
//! the fence epoch of its current claim (`claim_epoch`): acquisition writes it
//! (bumping the job version), every claim operation requires it to match its
//! own epoch before it commits against that job version, and an edit drops
//! it. A holder whose lease expired, was taken over, or whose job was edited
//! therefore cannot commit, whatever it read earlier.
#[path = "claims/acquisition.rs"]
mod acquisition;
mod completion;
#[path = "claims/manual_claim_reconciliation.rs"]
mod manual_claim_reconciliation;

use super::codec::{INCARNATION, doc_to_job, from_ms, incarnation, integer, run_id, run_to_doc};
use super::{CAS_ATTEMPTS, CronDocuments, JOBS, OWNER, RUNS, storage_error};
use anyhow::{Result, bail};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tinyflows_schedule::CronJob;
use tinystoragedrivers_core::{
    Capability, ErrorKind, Key, Leases, Precondition, Versioned, WriteOp, primitives,
};

const COMPLETED: &str = "completed_occurrence";
/// The job field holding the run number of the claim's queued placeholder.
const QUEUED: &str = "queued_run";
/// The logical owner paired with the active `claim_epoch` receipt. The lease
/// holder is a separate random token, so ambiguous admission can identify its
/// owner without weakening lease-token fencing.
const CLAIM_OWNER: &str = "claim_owner";
/// The job field holding the fence epoch of the claim that last acquired it.
/// Every acquisition rewrites the job with it, so a stale holder's
/// compare-and-swap on the job version fails, and an edit (which rebuilds the
/// document without it) invalidates every claim atomically.
const CLAIM_EPOCH: &str = "claim_epoch";

/// The receipt of a completed claim, kept on the job document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    owner: String,
    token: String,
    epoch: u64,
    incarnation: String,
    occurrence_ms: i64,
}

/// What a claim captured when it was acquired.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ClaimLease {
    owner: String,
    token: String,
    epoch: u64,
    incarnation: String,
    occurrence: DateTime<Utc>,
    started_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
}

/// A fenced occurrence captured before dispatch. Keep this value through
/// execution and pass it to completion; reading a fresh job loses that fence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaimedOccurrence {
    job: CronJob,
    lease: ClaimLease,
}
impl ClaimedOccurrence {
    /// The exact job snapshot acquired for dispatch.
    pub fn job(&self) -> &CronJob {
        &self.job
    }
    /// The worker that acquired this occurrence.
    pub fn owner(&self) -> &str {
        &self.lease.owner
    }
    /// The random fencing token (the lease holder), changed on each
    /// acquisition or recovery.
    pub fn token(&self) -> &str {
        &self.lease.token
    }
    /// The acquired job incarnation.
    pub fn incarnation(&self) -> &str {
        &self.lease.incarnation
    }
    /// The scheduled instant (millisecond precision).
    pub fn occurrence(&self) -> DateTime<Utc> {
        self.lease.occurrence
    }
    /// Start instant captured with this lease.
    pub fn started_at(&self) -> DateTime<Utc> {
        self.lease.started_at
    }
    /// The initial expiry. Renewal extends the stored lease; this snapshot
    /// remains the original acquisition receipt.
    pub fn expires_at(&self) -> DateTime<Utc> {
        self.lease.expires_at
    }
}
fn expiry(now: DateTime<Utc>, duration: Duration) -> Result<DateTime<Utc>> {
    if duration <= Duration::zero() {
        bail!("cron store: lease duration must be positive");
    }
    now.checked_add_signed(duration)
        .ok_or_else(|| anyhow::anyhow!("cron store: lease expiry overflow"))
}

/// The `tsd.leases` key of `job_id`'s occurrence at `occurrence_ms`.
fn lease_key(job_id: &str, occurrence_ms: i64) -> Result<String> {
    Ok(Key::of([job_id, occurrence_ms.to_string().as_str()])?
        .as_str()
        .to_owned())
}

/// The run number of the queued placeholder `doc` carries, if any.
fn queued_run(doc: &Value) -> Option<i64> {
    doc.get(QUEUED).and_then(Value::as_i64)
}

/// The write that removes the queued placeholder run `doc` carries.
pub(super) fn queued_run_cleanup(doc: &Value) -> Result<Vec<WriteOp>> {
    Ok(queued_run(doc)
        .map(|seq| WriteOp::Delete {
            collection: RUNS.into(),
            id: run_id(seq),
            precondition: Precondition::None,
        })
        .into_iter()
        .collect())
}

fn receipt(doc: &Value) -> Result<Option<Receipt>> {
    doc.get(COMPLETED)
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(Into::into)
}

impl CronDocuments {
    /// Leases whose clock is the host-supplied `now`, as the claim API always
    /// took the time from its caller.
    fn leases_at(&self, now: DateTime<Utc>) -> Leases {
        let ms = u64::try_from(now.timestamp_millis()).unwrap_or_default();
        Leases::new(&self.docs).with_clock(std::sync::Arc::new(move || ms))
    }

    /// Whether `claim` still holds the live lease of its occurrence on the
    /// current, same-incarnation job whose next run is that occurrence.
    async fn owns(
        &self,
        stored: &Versioned<Value>,
        claim: &ClaimedOccurrence,
        now: DateTime<Utc>,
    ) -> Result<bool> {
        if incarnation(&stored.doc) != Some(claim.lease.incarnation.as_str())
            || stored.doc.get(CLAIM_EPOCH).and_then(Value::as_u64) != Some(claim.lease.epoch)
            || doc_to_job(stored)?.next_run != claim.lease.occurrence
        {
            return Ok(false);
        }
        let key = lease_key(&claim.job.id, claim.lease.occurrence.timestamp_millis())?;
        let held = self
            .leases_at(now)
            .current(OWNER, &key)
            .await
            .map_err(storage_error)?;
        Ok(held.is_some_and(|held| {
            held.holder == claim.lease.token && held.epoch == claim.lease.epoch
        }))
    }

    /// Frees the live lease of the occurrence `job_doc` is due at, whoever
    /// holds it, so a job that was edited, removed or re-created can claim it
    /// again at once instead of waiting for the lease to expire. Correctness
    /// does not depend on it (the edit's own job write already invalidated
    /// every claim through `claim_epoch`), so a failure is logged, not fatal.
    /// A write that loses to the holder's renewal is retried.
    pub(super) async fn fence_claims(&self, job_id: &str, job_doc: &Value) {
        let Some(occurrence) = job_doc.get("next_run_ms").and_then(Value::as_i64) else {
            return;
        };
        let Ok(key) = lease_key(job_id, occurrence) else {
            return;
        };
        let Ok(id) = Key::of([OWNER, key.as_str()]) else {
            return;
        };
        for _ in 0..CAS_ATTEMPTS {
            let found = match self.docs.get(primitives::LEASES, id.as_str()).await {
                Ok(Some(found)) => found,
                Ok(None) => return,
                Err(error) => {
                    tracing::warn!(job_id, %error, "[cron-claims] reading a lease to free it failed");
                    return;
                }
            };
            let mut body = found.doc.clone();
            if body.get("expires_ms").and_then(Value::as_i64) == Some(0) {
                return;
            }
            body["expires_ms"] = json!(0);
            match self
                .docs
                .put(primitives::LEASES, id.as_str(), body, found.unchanged())
                .await
            {
                Ok(_) => return,
                // The holder renewed (or another writer moved the lease): read it again.
                Err(error) if error.kind() == ErrorKind::Conflict => {}
                Err(error) => {
                    tracing::warn!(job_id, %error, "[cron-claims] freeing a lease failed");
                    return;
                }
            }
        }
        tracing::warn!(
            job_id,
            "[cron-claims] a lease kept changing while it was freed"
        );
    }

    /// Appends a durable queued placeholder run while preserving the lease.
    /// The scheduler replaces this row with terminal history when it
    /// completes. `false` when the claim is no longer owned.
    ///
    /// # Errors
    /// Malformed state, unavailable transactions, exhausted retries, or backend failure.
    pub async fn queue_claimed_run(
        &self,
        claim: &ClaimedOccurrence,
        queued_at: DateTime<Utc>,
    ) -> Result<bool> {
        self.docs
            .capabilities()
            .require(Capability::Transactions)
            .map_err(storage_error)?;
        self.ensure().await?;
        for _ in 0..CAS_ATTEMPTS {
            let Some(stored) = self
                .docs
                .get(JOBS, &claim.job.id)
                .await
                .map_err(storage_error)?
            else {
                return Ok(false);
            };
            if !self.owns(&stored, claim, queued_at).await? {
                return Ok(false);
            }
            if queued_run(&stored.doc).is_some() {
                return Ok(true);
            }
            let seq = self.next_run_number().await?;
            let mut doc = stored.doc.clone();
            doc[QUEUED] = json!(seq);
            let run = run_to_doc(
                seq,
                &claim.job.id,
                Some(&claim.lease.incarnation),
                queued_at,
                queued_at,
                "queued",
                None,
                0,
                None,
            );
            let extra = vec![WriteOp::Put {
                collection: RUNS.into(),
                id: run_id(seq),
                doc: run,
                precondition: Precondition::Absent,
            }];
            match self
                .commit_job(&claim.job.id, Some(&stored), Some(doc), extra)
                .await
            {
                Ok(_) => return Ok(true),
                Err(error) if super::admission::conflict(&error) => {}
                Err(error) => return Err(error),
            }
        }
        bail!("cron store: occurrence changed while queueing run")
    }

    /// Extends an unexpired, still-owned occurrence. A stale worker receives
    /// false and must stop; it cannot renew a successor lease.
    ///
    /// # Errors
    /// Invalid duration, malformed state, exhausted retries, or backend failure.
    pub async fn renew_claim(
        &self,
        claim: &ClaimedOccurrence,
        now: DateTime<Utc>,
        duration: Duration,
    ) -> Result<bool> {
        let expires_at = expiry(now, duration)?;
        self.change_claim(claim, now, Some(expires_at)).await
    }
    /// Releases an unexpired claim without advancing it. The occurrence can
    /// then be acquired again with a new token.
    ///
    /// # Errors
    /// Malformed state, unavailable transactions, exhausted retries, or backend failure.
    pub async fn release_claim(
        &self,
        claim: &ClaimedOccurrence,
        now: DateTime<Utc>,
    ) -> Result<bool> {
        self.change_claim(claim, now, None).await
    }
    async fn change_claim(
        &self,
        claim: &ClaimedOccurrence,
        now: DateTime<Utc>,
        expires_at: Option<DateTime<Utc>>,
    ) -> Result<bool> {
        self.docs
            .capabilities()
            .require(Capability::Transactions)
            .map_err(storage_error)?;
        self.ensure().await?;
        let key = lease_key(&claim.job.id, claim.lease.occurrence.timestamp_millis())?;
        let leases = self.leases_at(now);
        for _ in 0..CAS_ATTEMPTS {
            let Some(stored) = self
                .docs
                .get(JOBS, &claim.job.id)
                .await
                .map_err(storage_error)?
            else {
                return Ok(false);
            };
            if !self.owns(&stored, claim, now).await? {
                return Ok(false);
            }
            // Releasing ends the claim: its queued placeholder goes with it,
            // while the lease is still held.
            if expires_at.is_none() && queued_run(&stored.doc).is_some() {
                let mut doc = stored.doc.clone();
                doc.as_object_mut()
                    .ok_or_else(|| anyhow::anyhow!("cron store: invalid job"))?
                    .remove(QUEUED);
                let extra = queued_run_cleanup(&stored.doc)?;
                match self
                    .commit_job(&claim.job.id, Some(&stored), Some(doc), extra)
                    .await
                {
                    Ok(_) => {}
                    Err(error) if super::admission::conflict(&error) => continue,
                    Err(error) => return Err(error),
                }
            }
            return self
                .change_lease(&leases, &key, claim, now, expires_at)
                .await;
        }
        bail!("cron store: occurrence kept changing during lease change")
    }

    /// Renews (`Some`) or releases (`None`) the claim's lease.
    async fn change_lease(
        &self,
        leases: &Leases,
        key: &str,
        claim: &ClaimedOccurrence,
        now: DateTime<Utc>,
        expires_at: Option<DateTime<Utc>>,
    ) -> Result<bool> {
        match expires_at {
            Some(expires_at) => {
                // Renewal never shortens an existing lease.
                let current = leases
                    .current(OWNER, key)
                    .await
                    .map_err(storage_error)?
                    .map_or(0, |held| held.expires_ms);
                let wanted = expires_at.timestamp_millis().max(current);
                let ttl_ms = (wanted - now.timestamp_millis()).max(1);
                Ok(leases
                    .renew(OWNER, key, &claim.lease.token, claim.lease.epoch, ttl_ms)
                    .await
                    .map_err(storage_error)?
                    .is_some())
            }
            None => leases
                .release(OWNER, key, &claim.lease.token, claim.lease.epoch)
                .await
                .map_err(storage_error),
        }
    }

    /// Deletes the job completed by this receipt only while its incarnation
    /// and completion token still match and no later acquisition or edit has
    /// invalidated it. Use this for `delete_after_run` after successful
    /// completion. A replaced or changed job returns `false`.
    ///
    /// Deletion and admission are atomic. History cleanup is a subsequent
    /// write restricted to the removed incarnation, preserving replacements.
    ///
    /// # Errors
    /// Malformed state, unavailable transactions, exhausted retries, or backend
    /// failure. A history-cleanup failure can occur after the job was deleted.
    pub async fn remove_completed_claim(&self, claim: &ClaimedOccurrence) -> Result<bool> {
        self.docs
            .capabilities()
            .require(Capability::Transactions)
            .map_err(storage_error)?;
        self.ensure().await?;
        for _ in 0..CAS_ATTEMPTS {
            let Some(stored) = self
                .docs
                .get(JOBS, &claim.job.id)
                .await
                .map_err(storage_error)?
            else {
                return Ok(false);
            };
            let Some(completed) = receipt(&stored.doc)? else {
                return Ok(false);
            };
            if completed.token != claim.lease.token
                || completed.owner != claim.lease.owner
                || completed.incarnation != claim.lease.incarnation
                || completed.epoch != claim.lease.epoch
                || completed.occurrence_ms != claim.lease.occurrence.timestamp_millis()
                || incarnation(&stored.doc) != Some(claim.lease.incarnation.as_str())
            {
                return Ok(false);
            }
            if self.remove_unchanged(&stored).await? {
                return Ok(true);
            }
        }
        bail!("cron store: completed occurrence kept changing during removal")
    }
}

/// The scheduled occurrence stored on a job document, in epoch milliseconds.
fn job_occurrence_ms(doc: &Value) -> Result<i64> {
    integer(doc, "next_run_ms")?
        .ok_or_else(|| anyhow::anyhow!("cron store: document has no `next_run_ms`"))
}
