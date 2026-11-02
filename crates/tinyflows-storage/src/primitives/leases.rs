//! [`Leases`]: time-limited ownership with a fencing epoch.
//!
//! # Why expiry is a body field
//!
//! A lease could carry its expiry as record metadata
//! ([`WriteOptions::expires_ms`](crate::WriteOptions)), which would hide it
//! once it ran out. It deliberately does not. A takeover must read the expired
//! record to learn its `epoch` and write `epoch + 1`; if the record vanished,
//! the next holder would restart at epoch 1 and a stale holder's
//! a fence token over epoch 1 would hold again. So the record stays, its
//! `expires_ms` field says when the lease lapses, and `epoch` only ever moves
//! forward.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::LEASES;
use super::repo::{Repo, RetryPolicy};
use crate::Key;
use crate::{Clock, system_clock};
use tinystoragedrivers_api::DocumentStore;
use tinystoragedrivers_types::{Result, StorageError};

/// The stored body: `{holder, epoch, expires_ms}` plus the optional
/// `endpoint` and `released` fields, both absent from records that never
/// used them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Body {
    holder: String,
    epoch: u64,
    expires_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    released: bool,
}

#[allow(clippy::trivially_copy_pass_by_ref, reason = "serde skip predicate")]
fn is_false(value: &bool) -> bool {
    !*value
}

/// The stored state of one lease, live or not, as [`Leases::inspect`] reads
/// it. [`Lease`] is a holder's proof; this is an observer's view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseRecord {
    /// Who holds (or last held) the lease.
    pub holder: String,
    /// Where the holder can be reached, when it said so at acquisition.
    pub endpoint: Option<String>,
    /// The fencing epoch of the holder's tenure.
    pub epoch: u64,
    /// Unix epoch milliseconds at which the lease lapses.
    pub expires_ms: i64,
    /// The holder let go cleanly. A released record is never live; a record
    /// that is merely past `expires_ms` was abandoned (crash, partition).
    pub released: bool,
}

impl LeaseRecord {
    /// Whether the record still excludes other holders at `now_ms`.
    #[must_use]
    pub fn is_live(&self, now_ms: i64) -> bool {
        !self.released && now_ms < self.expires_ms
    }

    fn from_body(body: Body) -> Self {
        Self {
            holder: body.holder,
            endpoint: body.endpoint,
            epoch: body.epoch,
            expires_ms: body.expires_ms,
            released: body.released,
        }
    }
}

/// What [`Leases::acquire_with`] asks for.
#[derive(Debug, Clone, Copy)]
pub struct Acquire<'a> {
    /// The lease namespace.
    pub owner: &'a str,
    /// The leased key within `owner`.
    pub key: &'a str,
    /// The acquiring holder.
    pub holder: &'a str,
    /// How long the lease lasts from now, in milliseconds.
    pub ttl_ms: i64,
    /// Where the holder can be reached, recorded on the lease.
    pub endpoint: Option<&'a str>,
    /// The epoch this very holder instance already holds for the key, if
    /// any. A live record of the same `holder` string is a re-entrant
    /// acquire (same epoch) only when its epoch equals this. With `None` it
    /// is a leftover of a dead earlier incarnation and is taken over at once
    /// (unclean). With a `Some` that no longer matches, the caller is a
    /// stale actor and gets [`Acquisition::Held`]: it must not evict the
    /// live replacement.
    pub held_epoch: Option<u64>,
}

/// The outcome of [`Leases::acquire_with`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Acquisition {
    /// The lease is the caller's.
    Granted {
        /// The lease just written.
        lease: Lease,
        /// The acquisition replaced a record nobody released and that the
        /// caller did not itself hold: the previous holder's work may be
        /// half done. `false` for a fresh key, a released record and a
        /// re-entrant acquire.
        previous_unclean: bool,
    },
    /// Another holder's lease is live.
    Held(LeaseRecord),
}

/// A lease as its holder sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    id: String,
    /// Who holds the lease.
    pub holder: String,
    /// Moves up each time a different holder takes the lease over, and stays
    /// put while one holder renews. This is the write-fence token.
    pub epoch: u64,
    /// Unix epoch milliseconds at which the lease lapses.
    pub expires_ms: i64,
    /// Where the holder said it can be reached (see [`Acquire::endpoint`]).
    pub endpoint: Option<String>,
}

impl Lease {
    fn from_body(id: String, body: Body) -> Self {
        Self {
            id,
            holder: body.holder,
            epoch: body.epoch,
            expires_ms: body.expires_ms,
            endpoint: body.endpoint,
        }
    }
}

/// Leases over [`LEASES`], one record per `(owner, key)`.
///
/// Time comes from an injectable `Clock` (Unix epoch milliseconds), so
/// tests drive expiry without sleeping. A lease is live while
/// `now < expires_ms`.
#[derive(Clone)]
pub struct Leases {
    repo: Repo<Body>,
    clock: Clock,
}

impl std::fmt::Debug for Leases {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Leases")
            .field("repo", &self.repo)
            .finish_non_exhaustive()
    }
}

impl Leases {
    /// Leases on `docs`, which must be bound to the scope that holds them
    /// (typically `cluster` for process placement), timed by the system clock.
    ///
    /// # Panics
    ///
    /// Never: the collection name is a valid constant.
    #[must_use]
    #[allow(clippy::expect_used, reason = "LEASES is a valid constant name")]
    pub fn new(docs: &Arc<dyn DocumentStore>) -> Self {
        Self {
            repo: Repo::new(docs, LEASES).expect("tsd.leases is a valid collection"),
            clock: Arc::new(system_clock),
        }
    }

    /// Time leases with `clock` instead of the system clock.
    #[must_use]
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    /// Replace the retry policy of the underlying compare-and-swap loop.
    #[must_use]
    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.repo = self.repo.with_retry(retry);
        self
    }

    fn now(&self) -> Result<i64> {
        i64::try_from((self.clock)()).map_err(|_| StorageError::invalid_input("clock out of range"))
    }

    fn deadline(&self, ttl_ms: i64) -> Result<(i64, i64)> {
        if ttl_ms <= 0 {
            return Err(StorageError::invalid_input("a lease ttl must be positive"));
        }
        let now = self.now()?;
        let expires = now
            .checked_add(ttl_ms)
            .ok_or_else(|| StorageError::invalid_input("lease ttl out of range"))?;
        Ok((now, expires))
    }

    /// Take the lease `(owner, key)` for `holder` for `ttl_ms` milliseconds.
    ///
    /// Succeeds when the lease is absent (epoch 1), expired or released
    /// (epoch + 1, even for the holder that had it, so a fence from the
    /// earlier tenure stays dead), or still live and held by `holder` (the
    /// epoch stays, the expiry is extended). Returns `None` while another
    /// holder's lease is live. Racing acquirers get exactly one winner.
    ///
    /// # Errors
    ///
    /// `InvalidInput` for a non-positive ttl, or a storage error.
    pub async fn acquire(
        &self,
        owner: &str,
        key: &str,
        holder: &str,
        ttl_ms: i64,
    ) -> Result<Option<Lease>> {
        let id = id(owner, key)?;
        let stored = self
            .repo
            .update(&id, |current| {
                let (now, expires_ms) = self.deadline(ttl_ms)?;
                let epoch = match current {
                    None => 1,
                    Some(lease) if lease.expires_ms > now && !lease.released => {
                        if lease.holder == holder {
                            lease.epoch
                        } else {
                            return Ok(None);
                        }
                    }
                    Some(lease) => lease
                        .epoch
                        .checked_add(1)
                        .ok_or_else(|| StorageError::backend("lease epoch overflow"))?,
                };
                Ok(Some(Body {
                    holder: holder.to_owned(),
                    epoch,
                    expires_ms,
                    endpoint: None,
                    released: false,
                }))
            })
            .await?;
        Ok(stored.map(|(body, _)| Lease::from_body(id, body)))
    }

    /// Extend a lease `holder` still holds by `ttl_ms` from now, keeping its
    /// epoch. `epoch` is the epoch of the [`Lease`] the caller acquired: a
    /// holder string is reusable, so an actor from an earlier tenure of the
    /// same holder must not be able to renew the current one. Returns `None`
    /// when the lease is gone, lapsed, held by someone else or at another
    /// epoch: the caller has lost it and must stop acting on it (renewing
    /// never resurrects a lapsed lease; call [`acquire`](Self::acquire)).
    ///
    /// # Errors
    ///
    /// `InvalidInput` for a non-positive ttl, or a storage error.
    pub async fn renew(
        &self,
        owner: &str,
        key: &str,
        holder: &str,
        epoch: u64,
        ttl_ms: i64,
    ) -> Result<Option<Lease>> {
        let id = id(owner, key)?;
        let stored = self
            .repo
            .update(&id, |current| {
                let (now, expires_ms) = self.deadline(ttl_ms)?;
                Ok(current
                    .filter(|lease| {
                        lease.holder == holder
                            && lease.epoch == epoch
                            && lease.expires_ms > now
                            && !lease.released
                    })
                    .map(|lease| Body {
                        expires_ms,
                        ..lease
                    }))
            })
            .await?;
        Ok(stored.map(|(body, _)| Lease::from_body(id, body)))
    }

    /// Give the lease up: only the holder of a live lease at `epoch` (the
    /// epoch of the [`Lease`] the caller acquired) can, and it is marked
    /// expired at once (the record and its epoch stay, so the next holder
    /// continues the epoch). Returns whether a lease was released; a stale
    /// tenure of the same holder gets `false` and leaves the live one alone.
    ///
    /// # Errors
    ///
    /// A storage error.
    pub async fn release(&self, owner: &str, key: &str, holder: &str, epoch: u64) -> Result<bool> {
        let id = id(owner, key)?;
        let now = self.now()?;
        let stored = self
            .repo
            .update(&id, |current| {
                Ok(current
                    .filter(|lease| {
                        lease.holder == holder
                            && lease.epoch == epoch
                            && lease.expires_ms > now
                            && !lease.released
                    })
                    .map(|lease| Body {
                        expires_ms: now,
                        released: true,
                        ..lease
                    }))
            })
            .await?;
        Ok(stored.is_some())
    }

    /// The live lease `(owner, key)`, or `None` when absent or lapsed.
    ///
    /// # Errors
    ///
    /// A storage error.
    pub async fn current(&self, owner: &str, key: &str) -> Result<Option<Lease>> {
        let id = id(owner, key)?;
        let now = self.now()?;
        Ok(self
            .repo
            .get(&id)
            .await?
            .filter(|found| found.doc.expires_ms > now && !found.doc.released)
            .map(|found| Lease::from_body(id, found.doc)))
    }

    /// The stored record of `(owner, key)`, live, lapsed or released. Unlike
    /// [`current`](Self::current) it does not hide a record that stopped
    /// excluding others, so an observer can tell a clean release from an
    /// abandoned lease and read the epoch a takeover would continue.
    ///
    /// # Errors
    ///
    /// A storage error.
    pub async fn inspect(&self, owner: &str, key: &str) -> Result<Option<LeaseRecord>> {
        let id = id(owner, key)?;
        Ok(self
            .repo
            .get(&id)
            .await?
            .map(|found| LeaseRecord::from_body(found.doc)))
    }

    /// Drop the record of `(owner, key)` if and only if it still belongs to
    /// the tenure `(holder, epoch)`, live or not. Returns whether a record was
    /// removed; a record at another holder or epoch (a takeover wrote it) is
    /// left alone.
    ///
    /// This is the compaction primitive for lease keys that are never
    /// acquired again (one lease per cron occurrence, per run, per claim),
    /// which would otherwise pile up: [`release`](Self::release) deliberately
    /// keeps the record so a takeover continues the epoch, and nothing else
    /// ever removes it.
    ///
    /// # Why dropping is safe
    ///
    /// tsd keeps expired and released leases for one reason: a takeover reads
    /// the old record and writes `epoch + 1`, so a stale holder's fence at the
    /// old epoch stays dead. The delete is conditional on holder and epoch, so
    /// it can only remove the very tenure the caller knows: any takeover bumps
    /// the epoch, and a record that moved on is not touched. A holder's fence token on a
    /// removed record refuses every write (a missing fence document is a
    /// failed check), so the tenure that was dropped is fenced out for good.
    ///
    /// What the delete gives up is the epoch history: a later acquire of the
    /// same key would restart at epoch 1. The caller therefore must only
    /// compact a key that nothing will acquire again (for a cron occurrence:
    /// once the job's next run has moved past it), or one whose protected
    /// resources no longer hold epoch-1 writers.
    ///
    /// # Errors
    ///
    /// A storage error, or a conflict past the retry budget.
    pub async fn compact(&self, owner: &str, key: &str, holder: &str, epoch: u64) -> Result<bool> {
        let id = id(owner, key)?;
        self.repo
            .delete_if(&id, |lease| lease.holder == holder && lease.epoch == epoch)
            .await
    }
}

fn id(owner: &str, key: &str) -> Result<String> {
    Ok(Key::of([owner, key])?.as_str().to_owned())
}

#[path = "leases/acquire_with.rs"]
mod acquire_with;

#[cfg(test)]
#[path = "leases_tests.rs"]
mod tests;
