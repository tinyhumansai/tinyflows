//! Durable graph builder and compile contract.
//!
//! This is the authoring entry point for the recursive graph runtime: a
//! [`GraphBuilder`] accumulates nodes, edges, conditional routing, and a reducer,
//! and [`GraphBuilder::compile`] validates that topology and freezes it into an
//! immutable [`crate::graph::CompiledGraph`]. Because a node handler can itself
//! drive another compiled graph or a sub-agent, the same builder API is what
//! both hand-written Rust and model-authored `.rag`/`.ragsh` programs lower into
//! when they assemble a workflow that may recurse into sub-workflows.
//!
//! See [`types`] for the builder data types. `compile` validates the topology
//! and freezes it into an immutable [`crate::graph::CompiledGraph`].

mod types;

pub(crate) use types::{Branch, BuilderNode, NodeMeta};
pub use types::{
    END, ForkId, GraphBuilder, GraphDefaults, NodeContext, NodeFuture, NodeHandler, Route,
    RouterFn, START,
};

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;

use crate::graph::command::NodeResult;
use crate::graph::compiled::CompiledGraph;
use crate::graph::error::{GraphError, Result};
use crate::graph::ids::{GraphId, NodeId};
use crate::graph::reducer::{OverwriteStateReducer, StateReducer};

/// A relief registration for a mixed fan-in barrier.
///
/// A waiting/barrier node (see [`GraphBuilder::add_waiting_edge`]) normally
/// activates only once *every* registered predecessor has arrived. When one
/// of those predecessors (`relief_node`) is only reachable via a conditional
/// branch out of `source`, and `source` routes elsewhere instead, that
/// predecessor never runs and the barrier would wait forever.
///
/// A [`BarrierRelief`] fixes that without weakening the barrier into a plain
/// edge (which would let a *taken* branch's downstream data race the merge
/// and get silently dropped): when `source` completes a superstep without
/// routing to `relief_node`, the executor registers a phantom arrival of
/// `relief_node` at `barrier_node`, so the barrier can still clear on the
/// predecessors that actually ran.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BarrierRelief {
    /// The brancher node whose conditional routing determines whether
    /// `relief_node` runs.
    pub source: NodeId,
    /// The conditional-only predecessor of `barrier_node` that `source` may
    /// or may not route to.
    pub relief_node: NodeId,
    /// The mixed fan-in (all-waiting) node gated on `relief_node`'s arrival.
    pub barrier_node: NodeId,
}

impl<State, Update> Default for GraphBuilder<State, Update>
where
    State: Clone + Send + Sync + 'static,
    Update: Send + 'static,
{
    fn default() -> Self {
        Self::new()
    }
}

mod implementation;

#[cfg(test)]
#[path = "builder_tests.rs"]
mod test;
