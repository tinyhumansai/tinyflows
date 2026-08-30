//! The engine's waiting primitive: one slice of a backoff.
//!
//! Two places in the engine wait without holding the executor: the retry
//! backoff between a failed attempt and the next one, and the `Reenter` backoff
//! a polling node (a `gate`) asks for between activations. Both chop their wait
//! into short slices so a cancel is seen promptly, and both take a slice from
//! here.
//!
//! # Why a wait has to yield, not merely elapse
//!
//! `futures_timer::Delay` arms its timer when it is **constructed** — `new`
//! computes the deadline and pushes it to the global timer thread — and its
//! `poll` returns `Ready` immediately if that thread has already fired it. So a
//! task descheduled between constructing the `Delay` and first polling it for
//! longer than the slice finds the wait already over, and completes it without
//! ever returning `Pending`: a "wait" during which the executor was never given
//! a turn.
//!
//! That is not a cosmetic difference. The engine runs on the caller's executor,
//! and a backoff is the only point at which it hands that executor back. On a
//! single-threaded runtime the background work a `gate` is waiting on — the
//! tasks a `spawn` node started — can *only* progress while the engine is
//! yielded. A backoff that skips the yield therefore returns the gate to a
//! world that has not moved: it sees the same unsettled tickets, spends another
//! poll against its bounded budget, and the run takes a different number of
//! super-steps than an identical run whose backoff did yield. Under enough load
//! a gate could burn its whole poll budget and time out having never once let
//! the tasks it waited on run.
//!
//! Yielding unconditionally makes the wait mean what it says, and makes the
//! number of polls a gate needs a property of the graph rather than of how the
//! OS happened to schedule the process.

use std::task::Poll;

/// Hands the executor exactly one turn.
///
/// Returns `Pending` on its first poll — waking itself first, so it is
/// re-queued behind whatever else is already ready — and `Ready` on its second.
/// Unconditional: it does not consult a timer, which is the whole point.
async fn yield_once() {
    let mut yielded = false;
    std::future::poll_fn(|cx| {
        if yielded {
            return Poll::Ready(());
        }
        yielded = true;
        cx.waker().wake_by_ref();
        Poll::Pending
    })
    .await;
}

/// Waits `ms` milliseconds, always giving the executor at least one turn.
///
/// The timer is armed before the yield rather than after, so the turn handed
/// over counts toward the wait instead of being added to it.
pub(super) async fn wait_slice(ms: u64) {
    let delay = futures_timer::Delay::new(std::time::Duration::from_millis(ms));
    yield_once().await;
    delay.await;
}

#[cfg(test)]
#[path = "backoff_tests.rs"]
mod tests;
