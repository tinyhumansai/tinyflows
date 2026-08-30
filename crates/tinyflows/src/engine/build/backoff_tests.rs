//! Unit tests for the backoff slice primitive.
//!
//! These pin the contract of [`yield_once`], which is what makes a backoff
//! hand the executor a turn whatever the timer did. The bug that contract
//! exists to prevent is an engine-level one and is covered end to end by
//! `a_gate_takes_the_same_number_of_polls_every_run` in `tests/fuzz_resume.rs`;
//! what is asserted here is the piece that can be checked deterministically.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use super::{wait_slice, yield_once};

/// A waker that counts how many times it was woken.
struct Counting(AtomicUsize);

impl Wake for Counting {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// A yield is `Pending` once, then `Ready` — and it wakes itself, so an
/// executor re-queues it rather than dropping the task.
///
/// The self-wake is half the contract: a `Pending` that never wakes is a hang,
/// not a yield.
#[test]
fn a_yield_is_pending_once_then_ready() {
    let counter = Arc::new(Counting(AtomicUsize::new(0)));
    let waker = Waker::from(counter.clone());
    let mut cx = Context::from_waker(&waker);
    let mut future = std::pin::pin!(yield_once());

    assert_eq!(
        future.as_mut().poll(&mut cx),
        Poll::Pending,
        "the first poll of a yield must hand the executor its turn"
    );
    assert_eq!(
        counter.0.load(Ordering::SeqCst),
        1,
        "a yield must wake itself, or the turn it hands over is never taken back"
    );
    assert_eq!(
        future.as_mut().poll(&mut cx),
        Poll::Ready(()),
        "a yield hands over exactly one turn, not a stream of them"
    );
}

/// A wait slice never completes on its first poll, however the timer resolved.
///
/// This is the property the whole module exists for: a slice whose timer had
/// already fired must still yield. Asserted against a zero-length slice, whose
/// deadline is already in the past the moment it is armed — the case most
/// likely to short-circuit.
#[test]
fn a_wait_slice_never_completes_on_its_first_poll() {
    let counter = Arc::new(Counting(AtomicUsize::new(0)));
    let waker = Waker::from(counter);
    let mut cx = Context::from_waker(&waker);
    let mut future = std::pin::pin!(wait_slice(0));

    assert_eq!(
        future.as_mut().poll(&mut cx),
        Poll::Pending,
        "an already-elapsed slice must still yield, or concurrent work never runs"
    );
}

/// And it does finish: the yield does not turn a bounded wait into a hang.
#[tokio::test]
async fn a_wait_slice_still_completes() {
    wait_slice(1).await;
}
