//! Native control-flow node executors: if / switch / merge / split_out /
//! transform. These are pure (no host capabilities) and are implemented in
//! stage A2. Each currently returns [`crate::error::EngineError::Unimplemented`].
//!
//! One module per node kind so parallel work can edit them without conflicts.

pub mod condition;
pub mod merge;
pub mod split_out;
pub mod switch;
pub mod transform;

pub use condition::ConditionNode;
pub use merge::MergeNode;
pub use split_out::SplitOutNode;
pub use switch::SwitchNode;
pub use transform::TransformNode;
