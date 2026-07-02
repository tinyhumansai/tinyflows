//! Native control-flow node executors: `condition`, `switch`, `merge`,
//! `split_out`, and `transform`. These are pure — they route and reshape data
//! within the engine and use no host capability.
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
