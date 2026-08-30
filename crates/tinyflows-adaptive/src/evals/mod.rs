//! Measuring whether the learning is real.
//!
//! Everything else in this crate is machinery for accumulating knowledge — a
//! ledger, lessons, scores, repaired variants, a promotion gate. None of it
//! answers the only question that matters about any of it: **does an episode
//! go better because earlier episodes happened?**
//!
//! That question has a shape, and getting the shape wrong is easy enough that
//! it is worth stating before any code. A success rate cannot answer it: on
//! ten unrelated tasks solved once each, a learning loop and a plain retry
//! loop produce identical numbers. Neither can a single arm: "it solved six of
//! six" is compatible with the ledger being decorative. What answers it is a
//! **family** of related tasks, run twice in the same order, where the only
//! difference between the runs is whether anything survives between episodes —
//! and the number to read is the [`Series::slope`]: attempts-to-success
//! falling as the family progresses.
//!
//! Two pieces, because that experiment needs two things this crate did not
//! have:
//!
//! * [`slope`] — the measurement. `Episode`, two arms, the slope, and a
//!   comparison that refuses to call a bend a win unless the arm also
//!   converges.
//! * [`arms`] — the control. A loop with learning off is not a loop with its
//!   ledger removed; [`Forgetful`] blanks the cross-episode reads and leaves
//!   the within-episode ones, which is the difference under test.
//!
//! What is deliberately *not* here is a task set. A family has to be chosen
//! for shared technique and checkable answers, and belongs to whoever runs the
//! eval — this crate is host-agnostic, and a Project Euler list baked into it
//! would be an opinion about the host's domain.

mod arms;
mod slope;

#[cfg(test)]
mod tests;

pub use arms::Forgetful;
pub use slope::{Episode, Experiment, LEARNING_OFF, LEARNING_ON, Outcome, Series, Verdict};
