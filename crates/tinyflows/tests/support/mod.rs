//! Shared helpers for the generative (property-based) test suite.
//!
//! Cargo compiles every `tests/*.rs` as its own crate, so anything shared has
//! to live in a module each of them declares with `mod support;`. Only the
//! files that use it pay for it.

#![allow(dead_code)]

pub mod graphgen;
