//! The engine's one execution-*gating* hook.
//!
//! [`RunObserver`](crate::observability::RunObserver) reports what a run did;
//! every one of its callbacks returns `()`, so it can watch a run and never
//! change one. That is the right contract for metrics, progress, and history —
//! and the wrong one for a debugger, which has to be able to say "stop here",
//! "use this value instead", and "pretend that failed".
//!
//! So this is the other half: a [`StepInterceptor`] is consulted before and
//! after every non-trigger node activation, and the [`StepAction`] it returns is
//! obeyed. It is what [`crate::testkit`] is built on, and a host may implement
//! it directly for a fault-injection harness of its own.
//!
//! Three things are worth knowing before implementing one.
//!
//! **It is not free-form.** The action vocabulary is deliberately small, and
//! each variant lands the activation back on an existing engine path — an
//! injected failure enters the node's own `on_error` policy, a replaced output
//! routes through the same port logic real output does. There is no variant
//! that lets an interceptor invent control flow the engine does not already
//! have.
//!
//! **An interceptor may block for as long as it likes.** The activation parks
//! and the rest of the super-step continues around it; nothing else in the run
//! is holding a lock on it. That is what makes a breakpoint expressible. It is
//! also what makes a careless interceptor able to park a run forever, so an
//! interceptor that waits on anything must have its own release —
//! [`crate::testkit::DebugController`] is fail-open on timeout, cancellation,
//! detach, and drop for exactly this reason.
//!
//! **Nothing here costs anything when unused.** The engine holds an
//! `Option<Arc<dyn StepInterceptor>>` and builds a [`StepFrame`] only when it is
//! `Some`, so a run with no interceptor pays two `Option` checks per activation
//! and constructs nothing.
//!
//! ```
//! use tinyflows::interception::{StepAction, StepFrame, StepInterceptor, StepPhase};
//!
//! /// Refuses to let one node's real executor run, standing in a fixed answer.
//! struct StubOneNode;
//!
//! #[async_trait::async_trait]
//! impl StepInterceptor for StubOneNode {
//!     async fn intercept(&self, frame: StepFrame<'_>) -> StepAction {
//!         if frame.phase == StepPhase::Before && frame.node.id == "flaky_http" {
//!             return StepAction::Replace {
//!                 items: vec![tinyflows::data::Item::new(serde_json::json!({ "status": 200 }))],
//!                 port: None,
//!             };
//!         }
//!         StepAction::Continue { state_patch: None }
//!     }
//! }
//! ```

use async_trait::async_trait;
use serde_json::Value;

use crate::caps::Capabilities;
use crate::data::Item;
use crate::error::EngineError;
use crate::model::Node;
use crate::nodes::{LaneContext, NodeOutput};

/// Where in a node activation an interception happens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum StepPhase {
    /// Before the node's first execution attempt, once its input has been
    /// resolved from the run state.
    ///
    /// Fires **once per activation**, not once per retry attempt: retries are
    /// the engine's business, and a breakpoint that fired three times because a
    /// node was configured with `retry.max_attempts: 3` would be reporting an
    /// implementation detail as if it were a decision.
    Before,
    /// After the retry loop settles, before the activation's
    /// [`ExecutionStep`](crate::observability::ExecutionStep) is recorded.
    ///
    /// Fires whether the node succeeded or exhausted its attempts, so this is
    /// the phase that can see a failure — and the only one that can rewrite what
    /// the run records.
    After,
}

/// What the engine does with an intercepted activation.
///
/// This is the difference between a [`StepInterceptor`] and a
/// [`RunObserver`](crate::observability::RunObserver): an observer callback
/// returns `()`, while this is obeyed.
#[non_exhaustive]
pub enum StepAction {
    /// Proceed unchanged.
    ///
    /// `state_patch`, when present, is merged into the run state this
    /// activation reads from — and into the update it later commits, so the
    /// edit is not silently lost the moment the node writes its slot.
    /// [`StepPhase::Before`] only: at `After` the state this activation will
    /// write is already decided, and a patch there would be a lie about what
    /// the node saw.
    Continue {
        /// A partial run state to deep-merge before the node executes.
        state_patch: Option<Value>,
    },
    /// Emit `items` on `port` instead of what would otherwise happen.
    ///
    /// At [`StepPhase::Before`] the node's executor is **not run** — nothing
    /// reaches the outside world. At [`StepPhase::After`] the executor already
    /// ran and its side effects already happened; only what the graph sees
    /// downstream changes. Both route through the same port and lane logic real
    /// output does.
    Replace {
        /// The items to emit in place of the node's own.
        items: Vec<Item>,
        /// The port to emit them on. `None` means the default `main` port.
        port: Option<String>,
    },
    /// Emit nothing on the default port.
    ///
    /// Downstream still runs, with no input from this node — which is what makes
    /// this "skip this node" rather than "abort this branch".
    Skip,
    /// Behave as if the executor had failed with `message`, entering the node's
    /// own `on_error` policy.
    ///
    /// Remaining retry attempts are **not** taken: an injected failure is a
    /// statement about the outcome, not about the transport, so it short-circuits
    /// to the policy rather than making a debugger sit through synthetic
    /// attempts. To watch the real retry path, do not inject — break on the
    /// genuine failure instead.
    Fail {
        /// The failure message, reported as an
        /// [`EngineError::Capability`](crate::error::EngineError::Capability).
        message: String,
    },
    /// Raise a real graph interrupt here, checkpointing the run.
    ///
    /// [`StepPhase::Before`] only in practice. An interrupt **discards this
    /// activation's state update and re-runs the node from the top** on resume
    /// (see [`NodeControl::Interrupt`](crate::nodes::NodeControl::Interrupt)),
    /// which is free before the node has run and doubles its side effects
    /// afterwards.
    Interrupt {
        /// Identifies the pause to the host and addresses the resume value back
        /// to this node.
        id: String,
        /// Host-facing description of what is being waited on.
        payload: Value,
    },
}

/// Everything an interceptor can see about one node activation.
///
/// Borrowed throughout: the engine builds no frame at all when no interceptor
/// is attached, and an interceptor that hands the frame to another task
/// snapshots the parts it needs rather than holding the borrow.
#[non_exhaustive]
pub struct StepFrame<'a> {
    /// Which side of execution this is.
    pub phase: StepPhase,
    /// The node being executed, including its **unresolved** config. For the
    /// config as this activation will actually read it, see
    /// [`resolved_config`](Self::resolved_config).
    pub node: &'a Node,
    /// The super-step driving this activation, counting from 0.
    pub step: usize,
    /// How many execution attempts have been consumed. Always `0` at
    /// [`StepPhase::Before`].
    pub attempts: u32,
    /// The input items resolved for this activation.
    pub input: &'a [Item],
    /// The `run` slice of the run state — trigger payload, declared inputs, and
    /// approvals.
    pub run: &'a Value,
    /// The `nodes` slice of the run state: every node that has completed so
    /// far, keyed by node id.
    pub nodes: &'a Value,
    /// The whole run state as this activation sees it.
    pub state: &'a Value,
    /// The parallel lane this activation belongs to, when a `scatter` opened
    /// one. `None` for an ordinary activation.
    pub lane: Option<&'a LaneContext>,
    /// The value a checkpointed resume delivered to this node, if this
    /// activation is the re-run of one that had interrupted.
    pub resume: Option<&'a Value>,
    /// What the node produced. `Some` at [`StepPhase::After`] when it succeeded.
    pub output: Option<&'a NodeOutput>,
    /// Why the node failed. `Some` at [`StepPhase::After`] when its attempts
    /// were exhausted.
    pub error: Option<&'a EngineError>,
}

impl StepFrame<'_> {
    /// The `=`-expression scope this activation binds its config against:
    /// `item`, `items`, `run`, `nodes`, and `inputs`.
    ///
    /// Built through the crate's single scope constructor, so it is the same
    /// object the node's own expressions are evaluated against rather than a
    /// reconstruction that can drift from it.
    #[must_use]
    pub fn scope(&self) -> Value {
        let item = self
            .input
            .first()
            .map(|i| i.json.clone())
            .unwrap_or(Value::Null);
        let items: Vec<Value> = self.input.iter().map(|i| i.json.clone()).collect();
        crate::nodes::build_expr_scope(item, items, self.run, crate::nodes::nodes_scope(self.nodes))
    }

    /// The node's config with every `=`-expression resolved against
    /// [`scope`](Self::scope), plus one
    /// [`NullResolution`](crate::expr::NullResolution) per expression that came
    /// back `null`.
    ///
    /// **Pure — this executes nothing.** It is safe to call at a
    /// [`StepPhase::Before`] pause, and it is the answer to "what is this node
    /// actually about to be handed", including which of its bindings are
    /// quietly null. A null binding is a legal value the engine will not
    /// complain about and the single most common reason a graph runs green and
    /// does nothing.
    #[must_use]
    pub fn resolved_config(&self) -> (Value, Vec<crate::expr::NullResolution>) {
        crate::expr::resolve_traced(&self.node.config, &self.scope())
    }
}

/// A host hook that can gate and rewrite node execution.
///
/// See the [module docs](self) for what this is for and what an implementation
/// owes the run. Implementations must be `Send + Sync`: the engine clones the
/// interceptor into every node handler, and those run across threads.
#[async_trait]
pub trait StepInterceptor: Send + Sync {
    /// Called before and after each non-trigger node activation.
    ///
    /// Returning [`StepAction::Continue`] with no patch is the inert answer and
    /// leaves the run byte-identical to one with no interceptor attached.
    async fn intercept(&self, frame: StepFrame<'_>) -> StepAction;

    /// Substitute the [`Capabilities`] one activation runs with.
    ///
    /// Returning `None` — the default — leaves the run's own capabilities in
    /// place, which is what every ordinary run does.
    ///
    /// This exists because "which node made this call?" has no other honest
    /// answer. A capability implementation is handed a slug and some arguments;
    /// it is never told who is calling, and under a parallel super-step several
    /// nodes are calling at once, so neither a shared "current node" cell nor
    /// the order calls arrive in can recover it. Giving each activation its own
    /// capability bundle attributes every call exactly, with no ambient state.
    ///
    /// It also buys per-node mocking as a side effect: a harness can stub one
    /// node's tool calls and leave the rest of the graph talking to the real
    /// thing.
    ///
    /// Called once per activation, only when the returned bundle would be used
    /// — never on a run with no interceptor attached.
    fn capabilities_for(&self, node: &Node, capabilities: &Capabilities) -> Option<Capabilities> {
        let _ = (node, capabilities);
        None
    }
}
