//! Native control-flow node executors: `condition`, `switch`, `merge`,
//! `split_out`, `transform`, `loop`, `void`, and `dedup`. Most of these are
//! pure — they route and reshape data within the engine and use no host
//! capability. `dedup` is the one exception: it filters items against durable
//! `StateStore` state (see [`dedup`] for why it lives here anyway).
//!
//! One module per node kind so parallel work can edit them without conflicts.

pub mod condition;
pub mod dedup;
pub mod gather;
pub mod loop_node;
pub mod merge;
pub mod scatter;
pub mod split_out;
pub mod switch;
pub mod transform;
pub mod void;

pub use condition::ConditionNode;
pub use dedup::DedupNode;
pub use gather::GatherNode;
pub use loop_node::{DEFAULT_MAX_ITERATIONS, LoopNode};
pub use merge::MergeNode;
pub use scatter::{MAX_LANES, ScatterNode};
pub use split_out::SplitOutNode;
pub use switch::SwitchNode;
pub use transform::TransformNode;
pub use void::VoidNode;
