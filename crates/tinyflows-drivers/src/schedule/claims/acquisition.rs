use super::*;
use tinystoragedrivers_core::Lease;
use uuid::Uuid;

impl CronDocuments {
    /// Acquires one enabled, due occurrence, or recovers its expired lease.
    /// Only one worker succeeds across handles sharing this scope. The host
    /// supplies now; clocks must agree within its lease tolerance.
    /// Recovery is at least once: a crash after an external send but before
    /// completion can repeat that send. This is not an HTTP exactly-once API.
    ///
    /// # Errors
    /// Invalid owner/duration, malformed state, unavailable transactions,
    /// exhausted contention retries, or backend failure.
    pub async fn claim_due_job(
        &self,
        job_id: &str,
        owner: &str,
        now: DateTime<Utc>,
        duration: Duration,
    ) -> Result<Option<ClaimedOccurrence>> {
        self.claim_occurrence(job_id, None, owner, now, duration, false)
            .await
    }

    /// Due-only claim that also verifies the caller's captured definition.
    ///
    /// # Errors
    /// As [`Self::claim_due_job`].
    pub async fn claim_due_job_for(
        &self,
        expected: &CronJob,
        owner: &str,
        now: DateTime<Utc>,
        duration: Duration,
    ) -> Result<Option<ClaimedOccurrence>> {
        self.claim_occurrence(&expected.id, Some(expected), owner, now, duration, false)
            .await
    }

    /// Atomically claims a saved definition for an explicit manual run,
    /// including disabled jobs and jobs scheduled in the future.
    ///
    /// # Errors
    /// As [`Self::claim_due_job`].
    pub async fn claim_job_now(
        &self,
        expected: &CronJob,
        owner: &str,
        now: DateTime<Utc>,
        duration: Duration,
    ) -> Result<Option<ClaimedOccurrence>> {
        self.claim_occurrence(&expected.id, Some(expected), owner, now, duration, true)
            .await
    }

    async fn claim_occurrence(
        &self,
        job_id: &str,
        expected: Option<&CronJob>,
        owner: &str,
        now: DateTime<Utc>,
        duration: Duration,
        force: bool,
    ) -> Result<Option<ClaimedOccurrence>> {
        if owner.trim().is_empty() {
            bail!("cron store: lease owner must not be empty");
        }
        let expires_at = expiry(now, duration)?;
        self.docs
            .capabilities()
            .require(Capability::Transactions)
            .map_err(storage_error)?;
        self.ensure().await?;
        let leases = self.leases_at(now);
        let ttl_ms = duration.num_milliseconds().max(1);
        for _ in 0..CAS_ATTEMPTS {
            let Some(stored) = self.docs.get(JOBS, job_id).await.map_err(storage_error)? else {
                return Ok(None);
            };
            let job = doc_to_job(&stored)?;
            if expected.is_some_and(|expected| {
                job.created_at != expected.created_at
                    || job.claim_generation != expected.claim_generation
                    || !job.same_execution_definition(expected)
                    || job.schedule != expected.schedule
                    || job.next_run != expected.next_run
            }) {
                return Ok(None);
            }
            if !force && (!job.enabled || job.next_run > now) {
                return Ok(None);
            }
            // Everything fallible that does not need the lease is done before
            // acquiring it, so no error path can leak one.
            let key = lease_key(job_id, job.next_run.timestamp_millis())?;
            let occurrence = from_ms(job_occurrence_ms(&stored.doc)?)?;
            let incarnation = incarnation(&stored.doc)
                .map(str::to_owned)
                .unwrap_or_else(|| Uuid::new_v4().to_string());
            let mut doc = stored.doc.clone();
            let body = doc
                .as_object_mut()
                .ok_or_else(|| anyhow::anyhow!("cron store: invalid job"))?;
            body.insert(INCARNATION.into(), Value::String(incarnation.clone()));
            body.insert(CLAIM_OWNER.into(), Value::String(owner.to_owned()));
            body.remove(COMPLETED);
            body.remove(QUEUED);
            let stale_queued = queued_run_cleanup(&stored.doc)?;
            let token = Uuid::new_v4().to_string();
            let Some(lease) = leases
                .acquire(OWNER, &key, &token, ttl_ms)
                .await
                .map_err(storage_error)?
            else {
                return Ok(None);
            };
            // Always write the job: its new version is what fences a stalled
            // earlier holder, and the epoch it carries is what every claim
            // operation checks. The lease is given back on any failure.
            doc[CLAIM_EPOCH] = json!(lease.epoch);
            match self
                .commit_job(job_id, Some(&stored), Some(doc), stale_queued)
                .await
            {
                Ok(_) => {}
                Err(error) if super::super::admission::conflict(&error) => {
                    self.give_back(&leases, &key, &lease).await;
                    continue;
                }
                Err(error) => {
                    self.give_back(&leases, &key, &lease).await;
                    return Err(error);
                }
            }
            return Ok(Some(ClaimedOccurrence {
                job,
                lease: ClaimLease {
                    owner: owner.into(),
                    token,
                    epoch: lease.epoch,
                    incarnation,
                    occurrence,
                    started_at: now,
                    expires_at,
                },
            }));
        }
        bail!("cron store: occurrence kept changing during acquisition")
    }

    async fn give_back(&self, leases: &Leases, key: &str, lease: &Lease) {
        if let Err(error) = leases.release(OWNER, key, &lease.holder, lease.epoch).await {
            tracing::warn!(%key, epoch = lease.epoch, %error, "[cron-claims] giving back an unused lease failed");
        }
    }
}
