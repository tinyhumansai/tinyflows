//! The tinyflows authoring **copilot**: the words, not the runtime.
//!
//! Two things live here, and both are properties of tinyflows rather than of
//! whichever host is running the turn:
//!
//! - [`prompts`] — the standing archetypes. `WORKFLOW_BUILDER` teaches the
//!   graph DSL, the node vocabulary, the binding syntax and the propose-only
//!   persistence contract; `FLOW_DISCOVERY` teaches how to ground a buildable
//!   automation idea in a user's own data.
//! - [`builder`] — one authoring request ([`builder::BuilderRequest`]) rendered
//!   into the natural-language brief that opens a builder turn
//!   ([`builder::render_prompt`]).
//!
//! # The harness is the host's
//!
//! This crate names no tool trait, no agent registry, no model client and no
//! storage. It takes a request and returns a string. A host supplies the turn
//! runner, the tools it is willing to expose, and the store the proposal is
//! eventually written to — and two hosts that agree on none of those still get
//! the same copilot.
//!
//! That split is why the persistence contract is stated *here* rather than
//! enforced here: every mode is propose-only, and a host that decides to accept
//! a proposal on the author's behalf is making its own decision, with its own
//! guards, on top of a brief that told the model not to.

pub mod builder;
pub mod prompts;

pub use builder::{BuildMode, BuilderRequest, render_prompt};
