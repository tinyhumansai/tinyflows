//! The standing prompt archetypes for the authoring agents.
//!
//! They are `include_str!`ed from Markdown beside this file rather than
//! embedded as string literals: they are long, they are edited as prose, and a
//! diff to a `.md` file reads as a change to what the copilot *says*, which is
//! exactly what it is.

/// The workflow-authoring archetype: the graph DSL, the node vocabulary, the
/// binding syntax, and the propose-only persistence contract.
pub const WORKFLOW_BUILDER: &str = include_str!("prompts/workflow_builder.md");

/// The read-only discovery archetype: how to ground a buildable automation idea
/// in the user's own data rather than inventing one.
pub const FLOW_DISCOVERY: &str = include_str!("prompts/flow_discovery.md");
