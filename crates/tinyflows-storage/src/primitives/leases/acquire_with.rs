//! Full host-semantics lease acquisition.

use super::*;

impl Leases {
    /// Take `(owner, key)` with the full host semantics: an endpoint, a
    /// distinction between a clean and an unclean takeover, and an unclean
    /// takeover of the same holder's leftover live record.
    ///
    /// The rule, in order:
    ///
    /// 1. no record: epoch 1, clean;
    /// 2. live, same holder and `held_epoch == Some(record.epoch)`:
    ///    re-entrant, the epoch stays, clean;
    /// 3. released: epoch + 1, clean;
    /// 4. live, same holder, `held_epoch == None` (the caller holds nothing:
    ///    the previous incarnation died before it expired): epoch + 1,
    ///    unclean, taken over at once, fencing the old incarnation out. A
    ///    caller that passes a `held_epoch` which no longer matches is a
    ///    stale actor and gets [`Acquisition::Held`] instead, and so does
    ///    a call that lost a race to another acquire of the same holder;
    /// 5. expired (`now >= expires_ms`): epoch + 1, unclean;
    /// 6. otherwise [`Acquisition::Held`] with the live record.
    ///
    /// Racing acquirers get exactly one winner.
    ///
    /// # Errors
    ///
    /// `InvalidInput` for a non-positive ttl, a storage error, or a backend
    /// error when the epoch cannot be advanced.
    pub async fn acquire_with(&self, request: Acquire<'_>) -> Result<Acquisition> {
        let Acquire {
            owner,
            key,
            holder,
            ttl_ms,
            endpoint,
            held_epoch,
        } = request;
        let id = id(owner, key)?;
        let mut unclean = false;
        let mut seen: Option<Body> = None;
        // `Repo::update` calls the closure again only after losing the swap,
        // so any call after the first means another writer got in: judge the
        // record as that competitor's write, never as a dead incarnation.
        let mut attempts = 0_u32;
        let stored = self
            .repo
            .update(&id, |current| {
                let (now, expires_ms) = self.deadline(ttl_ms)?;
                unclean = false;
                seen = None;
                let raced = attempts > 0;
                attempts += 1;
                let (epoch, was_unclean) = match &current {
                    None => (1, false),
                    Some(found) => {
                        let live = found.expires_ms > now && !found.released;
                        if live && found.holder == holder && held_epoch == Some(found.epoch) {
                            (found.epoch, false)
                        } else if live && (found.holder != holder || raced || held_epoch.is_some())
                        {
                            // Another holder, a competing call of the same
                            // holder that just won, or a caller proving it
                            // holds a stale epoch: the live record stands.
                            seen.clone_from(&current);
                            return Ok(None);
                        } else {
                            let next = found
                                .epoch
                                .checked_add(1)
                                .ok_or_else(|| StorageError::backend("lease epoch overflow"))?;
                            (next, !found.released)
                        }
                    }
                };
                unclean = was_unclean;
                Ok(Some(Body {
                    holder: holder.to_owned(),
                    epoch,
                    expires_ms,
                    endpoint: endpoint.map(str::to_owned),
                    released: false,
                }))
            })
            .await?;
        Ok(match stored {
            Some((body, _)) => Acquisition::Granted {
                lease: Lease::from_body(id, body),
                previous_unclean: unclean,
            },
            None => match seen {
                Some(body) => Acquisition::Held(LeaseRecord::from_body(body)),
                None => {
                    return Err(StorageError::backend(
                        "lease acquire declined without a live record",
                    ));
                }
            },
        })
    }
}
