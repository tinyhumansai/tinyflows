//! When a collector has waited long enough: the release policies shared by the
//! `gate` node and (later) the scatter/gather barrier.
//!
//! Both constructs answer the same question — *given how many of the things I am
//! waiting for have settled, do I go now?* — so the decision lives here once
//! rather than being re-derived, and slightly differently, in each. The two
//! differ only in what they are counting: lanes of a fan-out, or tickets of
//! spawned work.
//!
//! Deciding *when* to release is deliberately separate from deciding *what to
//! emit*. This module is pure and synchronous: it reads counters and returns a
//! verdict, which is what makes the policies exhaustively testable against a
//! reference implementation without running a graph.

use serde_json::Value;

use crate::error::{EngineError, Result};

/// How many arrivals a collector waits for before it proceeds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReleasePolicy {
    /// Wait for every one. The default, and the only policy that cannot emit a
    /// partial result.
    All,
    /// Go as soon as the first one settles.
    Any,
    /// Go as soon as `n` have settled, whichever they are.
    FirstN(usize),
    /// Go once `n` have settled — the same rule as [`Self::FirstN`], named
    /// separately because "a quorum of 3" and "the first 3" mean different
    /// things to whoever reads the workflow, and a later change to one should
    /// not silently change the other.
    Quorum(usize),
    /// Wait for every one, but settle for what arrived when the clock runs out.
    TimeoutPartial,
}

/// What a collector should do right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Release {
    /// Enough has arrived: proceed with what is in hand.
    Emit,
    /// Not yet: wait and ask again.
    Wait,
    /// The wait budget is spent and the policy does not accept a partial
    /// result. The caller routes this to its `timeout` port or `on_error`.
    Timeout,
}

impl ReleasePolicy {
    /// Reads a policy from a node's `config.release` / `config.n`.
    ///
    /// # Errors
    /// Returns [`EngineError::Capability`] for an unknown policy name, or for
    /// `first_n` / `quorum` without a positive `n` — both are authoring
    /// mistakes whose only sensible runtime default would be a lie about what
    /// the workflow says.
    pub(crate) fn from_config(config: &Value, node_id: &str) -> Result<Self> {
        let count = || -> Result<usize> {
            config
                .get("n")
                .and_then(Value::as_u64)
                .filter(|n| *n > 0)
                .and_then(|n| usize::try_from(n).ok())
                .ok_or_else(|| {
                    EngineError::Capability(format!(
                        "node {node_id:?}: `release` needs a positive integer `n`"
                    ))
                })
        };
        match config.get("release").and_then(Value::as_str) {
            None | Some("all") => Ok(Self::All),
            Some("any") => Ok(Self::Any),
            Some("first_n") => Ok(Self::FirstN(count()?)),
            Some("quorum") => Ok(Self::Quorum(count()?)),
            Some("timeout_partial") => Ok(Self::TimeoutPartial),
            Some(other) => Err(EngineError::Capability(format!(
                "node {node_id:?}: unknown `release` policy {other:?}; expected one of \
                 all, any, first_n, quorum, timeout_partial"
            ))),
        }
    }

    /// How many arrivals this policy needs, given `expected` in total.
    ///
    /// Clamped to `expected`: a `quorum` of 5 over 3 lanes would otherwise wait
    /// for an arrival that can never come, turning an over-specified workflow
    /// into a hang rather than a run that waits for everything.
    fn threshold(self, expected: usize) -> usize {
        match self {
            Self::All | Self::TimeoutPartial => expected,
            Self::Any => 1.min(expected),
            Self::FirstN(n) | Self::Quorum(n) => n.min(expected),
        }
    }

    /// Decides whether a collector holding `arrived` of `expected` should go.
    ///
    /// `budget_spent` says the wait budget is exhausted — the caller owns what
    /// that means (elapsed time, or a poll count), because a super-step engine
    /// measures waiting in activations rather than only in seconds.
    pub(crate) fn evaluate(self, arrived: usize, expected: usize, budget_spent: bool) -> Release {
        if arrived >= self.threshold(expected) {
            return Release::Emit;
        }
        if !budget_spent {
            return Release::Wait;
        }
        // Out of budget. `timeout_partial` is the one policy that would rather
        // proceed with less than it asked for than not proceed at all.
        match self {
            Self::TimeoutPartial => Release::Emit,
            _ => Release::Timeout,
        }
    }
}

#[cfg(test)]
#[path = "release_tests.rs"]
mod tests;
