//! The agent-facing surface: every capability in this module as a named tool.
//!
//! Workflows here are written by agents as often as by people, and an agent
//! that cannot debug what it wrote can only guess at why it failed. So the
//! testing and debugging capabilities are not only a Rust API — they are also a
//! set of [`ToolContract`]s with real JSON Schemas and a JSON-in/JSON-out
//! [`TestkitRegistry::dispatch`].
//!
//! **tinyflows registers nothing and talks to no model.** It says what the tools
//! are, what they do, and what they take; the host decides which to expose, to
//! whom, and under what name. That is the same division
//! [`crate::catalog`] already draws for the node-kind contracts.
//!
//! ```no_run
//! # async fn example() -> Result<(), tinyflows::testkit::tools::ToolError> {
//! use tinyflows::testkit::tools::{TestkitRegistry, all_tools};
//! use serde_json::json;
//!
//! // What a host hands its agent runtime.
//! for tool in all_tools() {
//!     println!("{} — {}", tool.name, tool.summary);
//! }
//!
//! // What it does with a call.
//! let registry = TestkitRegistry::new();
//! let result = registry
//!     .dispatch("flow_test.run", json!({ "graph": { /* … */ } }))
//!     .await?;
//! # let _ = result;
//! # Ok(())
//! # }
//! ```
//!
//! # Why the registry is stateful
//!
//! A debug session outlives one tool call by design: an agent pauses a run in
//! one turn, looks at it, and steps it in the next. The registry is what holds
//! the run in between. Sessions are reaped after
//! [`SESSION_IDLE_TIMEOUT`], so an agent that walks away does not hold a
//! spawned run for the life of the process.

mod contracts;
mod error;
mod registry;

pub use contracts::{ToolContract, all_tools, tool_for};
pub use error::{ToolError, ToolErrorCode};
pub use registry::{SESSION_IDLE_TIMEOUT, TestkitRegistry};
