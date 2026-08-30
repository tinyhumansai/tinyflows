//! Breakpoints: pausing a run, looking at it, and changing it.
//!
//! The engine already knows how to pause — a `requires_approval` gate raises a
//! real interrupt, checkpoints, and waits. That mechanism is for waiting on a
//! *person*, which can take days, so it ends the run and resumes it later by
//! re-running the interrupted node from the top.
//!
//! A breakpoint needs the opposite trade. It is waiting on someone looking at
//! the run right now, and it has to be able to do three things the interrupt
//! path structurally cannot:
//!
//! - **break after a node**, which the interrupt path cannot do safely at all,
//!   because resuming re-runs the node and fires its side effects a second time;
//! - **override what a node produced**, which needs the activation still on the
//!   stack with its output in hand;
//! - **be driven from another task**, so an agent can inspect, decide, and step
//!   across separate tool calls.
//!
//! So a breakpoint parks the activation in place and holds the run task alive,
//! and the run is owned by a [`DebugSession`] rather than by the caller's stack.
//! The cost, stated plainly: a session lives in one process and dies with it.
//! [`PauseMode::Durable`] is available for the one case where surviving a
//! restart matters more — a break *before* a node, where nothing has run yet and
//! the re-run is free.
//!
//! ```no_run
//! # async fn example(compiled: tinyflows::compiler::CompiledWorkflow) -> tinyflows::error::Result<()> {
//! use std::time::Duration;
//! use tinyflows::caps::mock::mock_capabilities;
//! use tinyflows::testkit::debug::{BreakpointSpec, DebugCommand, DebugSession};
//! use serde_json::json;
//!
//! let mut session = DebugSession::start_quiet(compiled, json!({}), mock_capabilities())?;
//! session
//!     .controller()
//!     .set_breakpoint(BreakpointSpec::before("send_email"))?;
//!
//! if let Some(pause) = session.next_pause(Duration::from_secs(5)).await {
//!     // What was this node about to be handed, and which bindings were empty?
//!     println!("{:?}", pause.null_bindings);
//!     session
//!         .controller()
//!         .release(pause.pause_id, DebugCommand::Continue)?;
//! }
//! let outcome = session.finish().await?;
//! # let _ = outcome;
//! # Ok(())
//! # }
//! ```

mod breakpoint;
mod controller;
mod session;

pub use breakpoint::{Breakpoint, BreakpointId, BreakpointSpec, Condition, NodeTarget, PauseMode};
pub use controller::{
    DEFAULT_PAUSE_TIMEOUT, DebugCommand, DebugController, PauseSnapshot, PauseStream,
};
pub use session::{DebugSession, SessionStatus};
