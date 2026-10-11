//! Cron jobs and their run history on a `tinystoragedrivers` [`DocumentStore`].
//!
//! The document-port twin of `tinyflows_sqlite::schedule`: the same functions,
//! as `async` methods on [`CronDocuments`] instead of free functions over a
//! [`CronStoreOptions`](https://docs.rs/tinyflows-sqlite), with the same
//! arguments, results and error messages. A host that opened a storage backend
//! (SQLite on a desktop, MongoDB in the cloud, memory in tests) hands it a
//! document handle, usually one already bound to a tenant scope.
//!
//! # Layout
//!
//! | Collection | Document id | Holds |
//! | --- | --- | --- |
//! | `cron_jobs` | job id (`flow:<flow_id>` for a flow-schedule job) | one job |
//! | `cron_runs` | zero-padded run number | one run of one job |
//! | `cron_counters` | `runs` | the next run number |
//!
//! A job's `schedule`, `delivery` and `origin` are JSON strings, so a MongoDB
//! backend never meets keys a caller chose. Optional fields are left out
//! rather than stored as `null`. Instants are kept twice: RFC 3339 text for
//! reading back, and epoch integers for ordering and range filters:
//! `created_ms`, `next_run_ms` plus `next_run_ns`, and `started_ns`
//! (nanoseconds, so jobs due and runs started within one millisecond still
//! order by their real instant).
//!
//! # Concurrency
//!
//! On a transaction-capable backend, every job mutation also CASes a shared
//! admission ledger in one atomic batch. Its optional enabled-job cap applies
//! to every upgraded handle, even one created before policy installation.
//! Legacy rows are imported with their versions fenced in the initialization
//! batch. Deploy upgraded writers together; older binaries or direct writes to
//! the job documents do not participate in admission and are unsupported once
//! a policy is installed.
//!
//! [`CronDocuments::claim_due_job`] captures a job incarnation, exact scheduled
//! occurrence and random lease token before dispatch. Renewal, release and
//! [`CronDocuments::complete_claim`] reject stale tokens, expired leases and
//! edited/canceled/re-created jobs. Completion records the incarnation-bound
//! run and advances the schedule atomically. Expired claims can be recovered:
//! execution is at least once, and a crash after an external HTTP send but
//! before completion can repeat delivery.
//!
//! The legacy [`CronDocuments::due_jobs`] API remains a read: callers that
//! dispatch its results without claiming can duplicate execution. Legacy
//! `record_run` and `reschedule_after_run` do not carry dispatch incarnations;
//! hosts needing incarnation fencing must use the claim API through completion.
//!
//! Nontransactional backends retain unlimited legacy CRUD when no admission
//! ledger exists. They cannot install policy or use occurrence claims. A
//! persisted ledger on such a backend fails closed instead of bypassing policy.
//! Policy and claims never fall back to local storage.
//!
//! History pruning and deletion of a removed job's runs remain separate writes.
//! A crash can leave excess or orphan runs; subsequent pruning and
//! [`CronDocuments::sweep_orphan_runs`] reclaim them.

use std::sync::Arc;

use anyhow::{Result, anyhow};
use serde_json::Value;
use tinystoragedrivers_core::{CollectionSpec, DocumentStore, IndexSpec, StorageError, Versioned};

mod admission;
mod claims;
mod codec;
mod jobs;
mod patch;
mod policy;
mod removal;
mod runs;
mod runtime;
#[cfg(test)]
mod test_support;

pub use claims::ClaimedOccurrence;

pub use tinyflows_schedule::{
    AgentJobSpec, MAX_CRON_OUTPUT_BYTES, TRUNCATED_OUTPUT_MARKER, truncate_cron_output,
};

/// Collection holding one document per job.
pub const JOBS: &str = "cron_jobs";
/// Collection holding one document per run.
pub const RUNS: &str = "cron_runs";
/// Collection holding the run-number counter.
pub const COUNTERS: &str = "cron_counters";

/// Compare-and-swap attempts before a contended write gives up.
const CAS_ATTEMPTS: usize = 32;

/// The cron job store over one (usually tenant-scoped) document handle.
#[derive(Debug, Clone)]
pub struct CronDocuments {
    docs: Arc<dyn DocumentStore>,
    max_run_history: usize,
    max_tasks: usize,
}

impl CronDocuments {
    /// A store over `docs` keeping 50 runs per job and returning at most 64
    /// due jobs per poll, as `CronStoreOptions::new` does.
    pub fn new(docs: Arc<dyn DocumentStore>) -> Self {
        Self {
            docs,
            max_run_history: 50,
            max_tasks: 64,
        }
    }

    /// The same store with a run-history cap and due-job batch size (each at
    /// least 1).
    #[must_use]
    pub fn with_limits(self, max_run_history: usize, max_tasks: usize) -> Self {
        Self {
            max_run_history: max_run_history.max(1),
            max_tasks: max_tasks.max(1),
            ..self
        }
    }

    /// The collections this store writes, with the indexes its queries use.
    pub fn collections() -> Vec<CollectionSpec> {
        vec![
            CollectionSpec::new(JOBS)
                .index(IndexSpec::new("by_next_run", ["next_run_ms"]))
                .index(IndexSpec::new("by_due", ["enabled", "next_run_ms"]))
                // A new name, not a widened `by_due`: MongoDB refuses to
                // redefine an existing index's keys under the same name.
                .index(IndexSpec::new("by_due_ns", ["enabled", "next_run_ns"]))
                .index(IndexSpec::new("by_name", ["name"]))
                .index(IndexSpec::new("by_flow", ["job_type", "command"])),
            CollectionSpec::new(RUNS)
                .index(IndexSpec::new("by_job", ["job_id", "started_ns", "seq"])),
            CollectionSpec::new(COUNTERS),
        ]
    }

    /// Declares [`Self::collections`] (idempotent). Every method calls it.
    pub async fn ensure(&self) -> Result<()> {
        for spec in Self::collections() {
            self.docs
                .ensure_collection(&spec)
                .await
                .map_err(storage_error)?;
        }
        self.migrate_policies().await?;
        Ok(())
    }
}

/// A storage failure as this store's error.
fn storage_error(error: StorageError) -> anyhow::Error {
    let message = format!("cron store: {error}");
    anyhow::Error::new(error).context(message)
}

/// The job-not-found error, worded as the SQLite store words it.
fn job_not_found(job_id: &str) -> anyhow::Error {
    anyhow!("Cron job '{job_id}' not found")
}

/// Applies `change` to document `id` of `collection` under compare-and-swap.
/// Returns what was stored, or `None` when the document is missing or
/// `change` returns `Ok(None)` (nothing to write).
async fn compare_job(
    store: &CronDocuments,
    collection: &str,
    id: &str,
    change: impl Fn(&Value) -> Result<Option<Value>>,
) -> Result<Option<Versioned<Value>>> {
    for _ in 0..CAS_ATTEMPTS {
        let Some(stored) = store
            .docs
            .get(collection, id)
            .await
            .map_err(storage_error)?
        else {
            return Ok(None);
        };
        let Some(next) = change(&stored.doc)? else {
            return Ok(None);
        };
        match store
            .commit_job(id, Some(&stored), Some(next.clone()), vec![])
            .await
        {
            Ok(results) => {
                let Some(tinystoragedrivers_core::WriteResult::Put { version }) = results.first()
                else {
                    anyhow::bail!("cron store: invalid atomic write result");
                };
                return Ok(Some(Versioned {
                    id: id.into(),
                    version: *version,
                    doc: next,
                }));
            }
            Err(error) if admission::conflict(&error) => {}
            Err(error) => return Err(error),
        }
    }
    Err(anyhow!(
        "cron store: {collection}/{id} kept changing under {CAS_ATTEMPTS} attempts"
    ))
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "claims_tests.rs"]
mod claims_tests;

#[cfg(test)]
#[path = "admission_tests.rs"]
mod admission_tests;

#[cfg(test)]
#[path = "claim_removal_tests.rs"]
mod claim_removal_tests;
