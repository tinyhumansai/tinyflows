//! Testing, mocking, and live debugging for workflows.
//!
//! A workflow that runs is not a workflow that works. The engine will happily
//! execute a graph whose every binding resolved to `null`, whose agent node
//! dispatched with an empty prompt, and whose failure was swallowed by an
//! `on_error` policy — and report all of it as success, because each of those
//! is a legal value rather than an error. What is missing is not execution. It
//! is the means to *interrogate* an execution.
//!
//! This module is that, in four layers, each usable on its own:
//!
//! - [`mocks`] — programmable capability doubles with a call log. Say what a
//!   tool returns, make the third call fail, and ask afterwards exactly what was
//!   sent where.
//! - [`trace`] — a structured record of a run: every node's input and output,
//!   every `=`-binding with the value it resolved to, every capability call, and
//!   the diagnosis of what a green run is hiding.
//! - [`harness`] — the ergonomic front door for a test: build, mock, run,
//!   assert.
//! - [`debug`] — breakpoints. Pause a run before or after a node, inspect what
//!   it was about to receive, override it, and step on.
//!
//! Everything is built on [`crate::interception`], the engine's one
//! execution-gating hook, so nothing here is special-cased inside the engine and
//! a host can build its own equivalent the same way.
//!
//! # For agents, not only for tests
//!
//! Workflows here are written by agents as often as by people, and an agent
//! that cannot debug what it wrote can only guess at why it failed. [`tools`]
//! exposes every capability above as a named tool with a JSON Schema and a
//! JSON-in/JSON-out handler, so a host can hand the whole of this module to an
//! agent without writing an adapter. tinyflows still talks to no model itself:
//! the host owns registration, and these are only descriptors and handlers.
//!
//! Behind the `testkit` feature, off by default.

pub mod debug;
pub mod harness;
pub mod mocks;
pub mod tools;
pub mod trace;

pub use debug::{BreakpointSpec, DebugCommand, DebugController, DebugSession, PauseSnapshot};
pub use harness::{TestHarness, TestRun};
pub use mocks::{CallLog, CallOutcome, CapCall, MockCaps, Respond, capability};
pub use tools::{TestkitRegistry, ToolContract, ToolError, all_tools};
pub use trace::{BindingTrace, RunTrace, RunTracer, TraceStatus, TraceStep};
