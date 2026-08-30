//! `void` — the terminal sink.
//!
//! A `void` node accepts items on `main`, discards them, and activates nothing.
//! It is the end of its branch, and that is the whole feature.
//!
//! # Why a node for something the graph could already do
//!
//! A branch could always dead-end: [`crate::engine`] lowers any node with no
//! outgoing edges straight to the state-graph's `END` sentinel, and `END` is
//! filtered out of routing, so it contributes nothing to the next super-step's
//! active set. Wiring nothing to a port has exactly the same effect.
//!
//! What was missing is the *statement*. A port left unwired reads identically
//! to a port someone forgot to wire, so an author could not declare "this
//! branch is a side effect; nothing downstream waits on it", and a reviewer
//! could not tell intent from an accident. A `void` says it in the graph, where
//! both the reader and [`crate::validate`] can see it.
//!
//! One place that ambiguity was resolved *against* the author: a branch inside
//! a [`scatter`](crate::model::NodeKind::Scatter) lane that dead-ends is a hard
//! validation error, because a lane activation never writes the node's
//! top-level slot and so a stranded lane branch produces a wrong answer rather
//! than a failure. `void` makes that invisibility the contract instead of the
//! accident, and is therefore the one dead end a lane may have.
//!
//! # What it is not
//!
//! It adds **no concurrency**. Everything upstream of a `void` still runs
//! inline in its own super-step; only the *result* is dropped. If you want work
//! to overlap, that is [`spawn`](crate::model::NodeKind::Spawn) and the
//! `TaskRunner` capability, not this.
//!
//! It performs **no drain and no cancellation** at run end. Abandoning a branch
//! here is exactly what an ungathered `spawn` ticket already does, which makes
//! `spawn` → `void` the explicit spelling of "no [`gate`] will ever collect
//! this, and I meant that".
//!
//! [`gate`]: crate::model::NodeKind::Gate
//!
//! # Configuration
//!
//! None. A `void` node's `name` is where the human reason goes ("Fire and
//! forget: audit log") — it is already required, and unlike a config key it is
//! rendered by [`crate::visualization`]. Config is ignored entirely, including
//! `=`-expressions, so this node can emit no binding diagnostics.
//!
//! # What it leaves behind
//!
//! `{ "items": [], "port": null, "discarded": <n> }` in its run-state slot.
//! Emitting nothing would otherwise be indistinguishable from never having run,
//! since a node that never ran has no slot at all — so the three cases stay
//! separable:
//!
//! | slot | meaning |
//! |---|---|
//! | absent (`null`) | never activated |
//! | `discarded: 0` | activated, had nothing to drop |
//! | `discarded: 3` | activated, dropped three items |
//!
//! `discarded` counts **this activation's** input, not a running total. Two
//! consequences worth knowing: inside a `loop` body it is overwritten every
//! iteration, so the value that survives is the last one; and inside a scatter
//! lane it lands at `nodes.<id>.lanes.<lane>.discarded` rather than at the top
//! level, because a lane activation deliberately never writes the top-level
//! slot. A cumulative counter was considered and rejected for exactly those two
//! reasons — it would silently mean something different in each context.

use async_trait::async_trait;
use serde_json::json;

use crate::error::Result;
use crate::nodes::{NodeContext, NodeExecutor, NodeOutput};

/// Stable `tracing` grep prefix for every log line this node emits.
const LOG_PREFIX: &str = "[void]";

/// Terminal sink: discards its input and activates no successors.
///
/// See the [module docs](self) for why an explicit node beats an unwired port.
#[derive(Debug, Default, Clone)]
pub struct VoidNode;

#[async_trait]
impl NodeExecutor for VoidNode {
    async fn execute(&self, ctx: NodeContext<'_>) -> Result<NodeOutput> {
        let discarded = ctx.input.len();
        tracing::debug!(
            node = %ctx.node.id,
            discarded,
            "{LOG_PREFIX} discarding items; nothing downstream runs"
        );
        // No port: emitting on one would imply a successor could match it, and
        // `validate` guarantees there is none. The `discarded` count is the only
        // trace the node leaves, and it is what separates "ran on nothing" from
        // "never ran" (see the module docs).
        Ok(NodeOutput::empty().with_meta(json!({ "discarded": discarded })))
    }
}

#[cfg(test)]
#[path = "void_tests.rs"]
mod tests;
