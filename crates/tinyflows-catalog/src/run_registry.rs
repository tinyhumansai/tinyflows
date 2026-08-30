//! Process-local registry of in-flight flow runs, keyed by `run_id`
//! (== the run's checkpointer `thread_id`), so `flows_cancel_run` (issue G4)
//! can signal a synchronously-executing run to abort.
//!
//! A `flows_run` / `flows_resume` executes inline inside its RPC await (or a
//! fire-and-forget `tokio::spawn` from `flows::bus`), so there is no
//! `JoinHandle` a caller can reach. Instead each active run [`register`]s a
//! [`tokio_util::sync::CancellationToken`] here for the duration of the run and
//! `tokio::select!`s its future against the token's `cancelled()`. A separate
//! `flows_cancel_run` RPC looks the token up by `run_id` and [`cancel`]s it,
//! tripping the run's select arm.
//!
//! The registration is RAII: [`register`] returns a [`RunGuard`] that removes
//! the entry on `Drop` (including on panic / early return), so a finished run
//! can never leave a stale token wedged in the map.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

use tokio_util::sync::CancellationToken;

/// Monotonic registration id, so a [`RunGuard`] can prove an entry is still
/// *its own* before removing it. See [`RunGuard::drop`].
static NEXT_REGISTRATION: AtomicU64 = AtomicU64::new(1);

/// The live in-flight runs: `run_id` → (registration id, cancellation token).
static IN_FLIGHT: LazyLock<Mutex<HashMap<String, (u64, CancellationToken)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Registers `run_id` as in-flight and returns both a clone of its
/// cancellation token (to `select!` on) and a [`RunGuard`] that deregisters it
/// on drop. Hold the guard for the whole run.
///
/// A duplicate `run_id` replaces the prior token, and each registration carries
/// a unique id so a guard only ever removes the entry it installed.
///
/// This used to be documented as impossible ("thread ids are UUID-suffixed"),
/// which held while only `flows_run` / `flows_run_detached` registered — both
/// mint a fresh UUID `thread_id` per call, so two concurrent registrations could
/// never collide on a key. `flows_resume` is the first caller to register
/// against a **stable, pre-existing** id (the parked run's own), and nothing
/// serializes two concurrent resumes of the same run — a client double-submit or
/// a retry-on-timeout is enough. With removal keyed only by `run_id`, the loser
/// of that race would deregister the *winner's* live token on its way out, and a
/// later `flows_cancel_run` would then see `is_in_flight == false` for a run that
/// is genuinely executing, take its "parked/stale" branch, and drop the
/// checkpoint out from under it — the exact bug class this registry exists to
/// prevent.
pub fn register(run_id: &str) -> (CancellationToken, RunGuard) {
    let token = CancellationToken::new();
    let registration = NEXT_REGISTRATION.fetch_add(1, Ordering::Relaxed);
    let displaced = IN_FLIGHT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(run_id.to_string(), (registration, token.clone()));
    if displaced.is_some() {
        tracing::warn!(
            target: "flows",
            run_id,
            registration,
            "[flows] run_registry: duplicate registration for a run id already in flight — the displaced guard will no longer deregister this entry"
        );
    }
    tracing::debug!(target: "flows", run_id, registration, "[flows] run_registry: registered in-flight run");
    (
        token,
        RunGuard {
            run_id: run_id.to_string(),
            registration,
        },
    )
}

/// Signals the in-flight run keyed by `run_id` to cancel, if one is registered.
/// Returns `true` when a live run was signalled, `false` when no run with that
/// id is currently in flight (e.g. it already settled, or is a parked
/// `pending_approval` row with no executing task).
pub fn cancel(run_id: &str) -> bool {
    let guard = IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
    match guard.get(run_id) {
        Some((_registration, token)) => {
            token.cancel();
            tracing::info!(target: "flows", run_id, "[flows] run_registry: signalled in-flight run to cancel");
            true
        }
        None => {
            tracing::debug!(target: "flows", run_id, "[flows] run_registry: no in-flight run to cancel");
            false
        }
    }
}

/// Returns `true` when `run_id` is currently registered as an in-flight run in
/// THIS process. Used by the boot-time orphan sweep (bug B42) to distinguish a
/// genuinely orphaned `running` row (left by a prior process — not in flight)
/// from one a freshly-started run in this process legitimately owns, so the
/// sweep never reconciles a live run out from under itself.
pub fn is_in_flight(run_id: &str) -> bool {
    IN_FLIGHT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(run_id)
}

/// RAII guard that removes a run's entry from the in-flight registry on drop.
pub struct RunGuard {
    run_id: String,
    registration: u64,
}

impl Drop for RunGuard {
    /// Removes this run's entry — but only if it is still the entry THIS guard
    /// installed. A plain `remove(&run_id)` would let the loser of a duplicate
    /// registration deregister the winner's live token (see [`register`]).
    fn drop(&mut self) {
        let mut map = IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
        match map.get(&self.run_id) {
            Some((registration, _)) if *registration == self.registration => {
                map.remove(&self.run_id);
                tracing::debug!(target: "flows", run_id = %self.run_id, registration = self.registration, "[flows] run_registry: deregistered run");
            }
            Some(_) => {
                tracing::debug!(
                    target: "flows",
                    run_id = %self.run_id,
                    registration = self.registration,
                    "[flows] run_registry: entry belongs to a newer registration — leaving it in place"
                );
            }
            None => {}
        }
    }
}

#[cfg(test)]
#[path = "run_registry_tests.rs"]
mod tests;
