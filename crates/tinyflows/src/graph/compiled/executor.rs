//! Public run/resume entry points and the superstep execution engine.
//!
//! Split out of `compiled/mod.rs`; see that module's doc comment for the
//! full executor design (superstep loop, concurrency, and resumable-failure
//! semantics).

mod api;
mod node_execution;
mod outcomes;
mod persistence;
mod run_loop;
