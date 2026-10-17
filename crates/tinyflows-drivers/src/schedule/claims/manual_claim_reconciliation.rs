//! Fence an ambiguous manual admission without changing a replacement job.
use super::*;

impl CronDocuments {
    /// Verify that manual occurrence admission is available before a host
    /// starts an operation whose callback response may be ambiguous.
    pub fn require_manual_claim_support(&self) -> Result<()> {
        self.docs
            .capabilities()
            .require(Capability::Transactions)
            .map_err(storage_error)
    }

    /// Reconcile an ambiguous manual-claim response. A live lease is an
    /// admitted claim only when its epoch is recorded on the matching job
    /// document. Otherwise a same-document CAS fences the captured snapshot;
    /// any observed uncommitted lease is released by its exact token and epoch.
    pub async fn reconcile_manual_claim_admission(
        &self,
        expected: &CronJob,
        owner: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<ClaimedOccurrence>> {
        self.require_manual_claim_support()?;
        if owner.trim().is_empty() {
            bail!("cron store: lease owner must not be empty");
        }
        let key = lease_key(&expected.id, expected.next_run.timestamp_millis())?;
        for _ in 0..CAS_ATTEMPTS {
            let Some(stored) = self
                .docs
                .get(JOBS, &expected.id)
                .await
                .map_err(storage_error)?
            else {
                return Ok(None);
            };
            let job = doc_to_job(&stored)?;
            let leases = self.leases_at(now);
            let current_lease = leases.current(OWNER, &key).await.map_err(storage_error)?;
            let expected_matches = job.created_at == expected.created_at
                && job.claim_generation == expected.claim_generation
                && job.same_execution_definition(expected)
                && job.schedule == expected.schedule
                && job.next_run == expected.next_run;
            let committed_epoch = stored.doc.get(CLAIM_EPOCH).and_then(Value::as_u64);
            let committed_owner = stored.doc.get(CLAIM_OWNER).and_then(Value::as_str);

            if let Some(current_lease) = current_lease.as_ref() {
                if committed_epoch == Some(current_lease.epoch)
                    && incarnation(&stored.doc).is_some()
                    && job.next_run == expected.next_run
                {
                    // The stored logical owner is distinct from the lease's
                    // random holder token. Preserve any committed claim that
                    // belongs to another worker (or predates owner receipts).
                    if committed_owner != Some(owner) {
                        return Ok(None);
                    }
                    let started_at = if let Some(seq) = queued_run(&stored.doc) {
                        let id = run_id(seq);
                        self.docs
                            .get(RUNS, &id)
                            .await
                            .map_err(storage_error)?
                            .and_then(|run| run.doc.get("started_ms").and_then(Value::as_i64))
                            .and_then(|ms| from_ms(ms).ok())
                            .unwrap_or(now)
                    } else {
                        now
                    };
                    let Some(incarnation) = incarnation(&stored.doc) else {
                        continue;
                    };
                    return Ok(Some(ClaimedOccurrence {
                        job,
                        lease: ClaimLease {
                            owner: owner.to_owned(),
                            token: current_lease.holder.clone(),
                            epoch: current_lease.epoch,
                            incarnation: incarnation.to_owned(),
                            occurrence: expected.next_run,
                            started_at,
                            expires_at: from_ms(current_lease.expires_ms)?,
                        },
                    }));
                }
                if committed_epoch == Some(current_lease.epoch) {
                    // A committed claim with a different logical owner is
                    // never fenced or released by this reconciliation.
                    return Ok(None);
                }
            }

            if !expected_matches {
                // A changed or replaced job already fences a delayed claim's
                // captured document version. Do not rewrite that definition.
                return Ok(None);
            }

            let generation = job
                .claim_generation
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("cron store: claim generation exhausted"))?;
            let mut doc = stored.doc.clone();
            doc["claim_generation"] = generation.into();
            match self
                .commit_job(&job.id, Some(&stored), Some(doc), vec![])
                .await
            {
                Ok(_) => {
                    if let Some(current_lease) = current_lease.as_ref() {
                        // The committed row version fenced the delayed claim
                        // CAS. Release only the exact lease observed before
                        // that barrier; the token and epoch spare successors.
                        let _ = leases
                            .release(OWNER, &key, &current_lease.holder, current_lease.epoch)
                            .await
                            .map_err(storage_error)?;
                    }
                    return Ok(None);
                }
                Err(error) if super::super::admission::conflict(&error) => {}
                Err(error) => return Err(error),
            }
        }
        bail!("cron store: manual admission reconciliation kept changing")
    }
}
