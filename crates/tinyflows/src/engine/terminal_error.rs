//! The structured error a failed run reports, chosen independently of timing.
//!
//! A `stop`-policy node failure ends the run, but the runtime can only carry a
//! string out of a node handler, so the handler also leaves its real
//! [`EngineError`] in a shared slot for the entry point to return. A parallel
//! superstep can fail on several branches at once, and they reach the slot in
//! completion order — so the slot must not simply keep whichever came first.

use std::sync::{Arc, Mutex};

use crate::error::EngineError;

/// A run-ending node failure, with the position it was raised at.
pub(crate) struct TerminalError {
    step: usize,
    branch: usize,
    pub(crate) error: EngineError,
}

/// The slot every node handler of one run shares.
pub(crate) type TerminalSlot = Arc<Mutex<Option<TerminalError>>>;

/// Offers `error`, raised at superstep `step` by the activation at index
/// `branch` of that superstep's active set, as the run's reported error.
///
/// The rule: the earliest superstep wins, and within one superstep the lowest
/// active-set index wins. That index is the order the runtime folds parallel
/// branches in, and the order it already uses to name a failed step, so the
/// structured error and the runtime's own step error agree. Completion order
/// never matters: an earlier superstep's error is never replaced by a later
/// one, whatever its index. A sequential step has no fork and reports index 0;
/// it stops at its first failure, so it offers at most one error per step.
pub(crate) fn offer(
    slot: &mut Option<TerminalError>,
    step: usize,
    branch: usize,
    error: EngineError,
) {
    if slot
        .as_ref()
        .is_none_or(|held| (step, branch) < (held.step, held.branch))
    {
        *slot = Some(TerminalError {
            step,
            branch,
            error,
        });
    }
}

#[cfg(test)]
#[path = "terminal_error_tests.rs"]
mod tests;
