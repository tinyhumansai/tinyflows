use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::interception::{StepFrame, StepPhase};

/// Identifies a breakpoint within one
/// [`DebugController`](super::DebugController).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BreakpointId(pub u64);

/// Which nodes a breakpoint applies to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum NodeTarget {
    /// One node, by id.
    Id(String),
    /// Every node in the graph. What "step" is built from.
    Any,
}

impl NodeTarget {
    fn matches(&self, node_id: &str) -> bool {
        match self {
            Self::Id(id) => id == node_id,
            Self::Any => true,
        }
    }
}

/// When a breakpoint fires, beyond matching its node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Condition {
    /// Every matching activation.
    Always,
    /// Only when the activation failed.
    ///
    /// Never matches at [`StepPhase::Before`], where no error exists yet — a
    /// silent no-match rather than an error, because "break where it breaks" is
    /// a reasonable thing to ask for without specifying a phase.
    OnError,
    /// Only the `n`-th activation of this node in this run, counting from 1.
    ///
    /// What to reach for on a node inside a loop: break on the third pass
    /// rather than the first.
    Activation(u32),
    /// Only when this `=`-expression is truthy against the activation's scope.
    ///
    /// jq's notion of truthy — anything but `null` and `false`. The same
    /// expression dialect node config uses, so `=.items | length > 3` and
    /// `=nodes.fetch.item.status == "error"` both work.
    Expr(String),
    /// Every listed condition holds.
    All(Vec<Condition>),
    /// Any listed condition holds.
    Any(Vec<Condition>),
}

impl Condition {
    /// Whether this condition holds for `frame`, given which activation of the
    /// node this is.
    pub(super) fn holds(&self, frame: &StepFrame<'_>, activation: u32) -> bool {
        match self {
            Self::Always => true,
            Self::OnError => frame.error.is_some(),
            Self::Activation(n) => *n == activation,
            Self::Expr(expr) => {
                let value = crate::expr::resolve(&json!(expr), &frame.scope());
                !matches!(value, Value::Null | Value::Bool(false))
            }
            Self::All(all) => all.iter().all(|c| c.holds(frame, activation)),
            Self::Any(any) => any.iter().any(|c| c.holds(frame, activation)),
        }
    }
}

/// How a breakpoint parks the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PauseMode {
    /// Park the activation in place, holding the run task alive.
    ///
    /// Works at both phases, supports every [`DebugCommand`](super::DebugCommand),
    /// needs no checkpointer — and dies with the process.
    #[default]
    Live,
    /// Raise a real interrupt, checkpoint, and end the run; the decision is
    /// delivered on the next resume.
    ///
    /// Survives a process restart, but the node **re-runs from the top**, so it
    /// is refused for [`StepPhase::After`] where that would repeat the node's
    /// side effects. See
    /// [`set_breakpoint`](super::DebugController::set_breakpoint).
    Durable,
}

/// A breakpoint to register.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BreakpointSpec {
    /// Which node(s).
    pub target: NodeTarget,
    /// Break before the node executes.
    pub before: bool,
    /// Break after it settles.
    pub after: bool,
    /// The extra predicate.
    pub condition: Condition,
    /// Live or durable.
    pub mode: PauseMode,
    /// Fire at most this many times, then disable itself. `None` is unlimited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_hits: Option<u32>,
}

impl BreakpointSpec {
    /// Break before `node_id` executes.
    #[must_use]
    pub fn before(node_id: impl Into<String>) -> Self {
        Self {
            target: NodeTarget::Id(node_id.into()),
            before: true,
            after: false,
            condition: Condition::Always,
            mode: PauseMode::Live,
            max_hits: None,
        }
    }

    /// Break after `node_id` settles.
    #[must_use]
    pub fn after(node_id: impl Into<String>) -> Self {
        Self {
            after: true,
            before: false,
            ..Self::before(node_id)
        }
    }

    /// Break after any node that failed.
    ///
    /// The breakpoint to set when you do not yet know where the problem is.
    #[must_use]
    pub fn on_error() -> Self {
        Self {
            target: NodeTarget::Any,
            before: false,
            after: true,
            condition: Condition::OnError,
            mode: PauseMode::Live,
            max_hits: None,
        }
    }

    /// Narrow this breakpoint with a condition.
    #[must_use]
    pub fn when(mut self, condition: Condition) -> Self {
        self.condition = condition;
        self
    }

    /// Fire once, then disable.
    #[must_use]
    pub fn once(mut self) -> Self {
        self.max_hits = Some(1);
        self
    }

    /// Pause durably, through a checkpointed interrupt.
    #[must_use]
    pub fn durable(mut self) -> Self {
        self.mode = PauseMode::Durable;
        self
    }

    /// Whether this spec applies at `phase`.
    pub(super) fn covers(&self, phase: StepPhase) -> bool {
        match phase {
            StepPhase::Before => self.before,
            StepPhase::After => self.after,
        }
    }

    /// Whether this spec matches `frame`, ignoring hit limits.
    pub(super) fn matches(&self, frame: &StepFrame<'_>, activation: u32) -> bool {
        self.covers(frame.phase)
            && self.target.matches(&frame.node.id)
            && self.condition.holds(frame, activation)
    }
}

/// A registered breakpoint and its running hit count.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Breakpoint {
    /// Its id in this controller.
    pub id: BreakpointId,
    /// What was asked for.
    pub spec: BreakpointSpec,
    /// How many times it has fired.
    pub hits: u32,
    /// Whether it still fires. A breakpoint that reached its `max_hits`
    /// disables itself rather than being removed, so it is still visible in a
    /// listing.
    pub enabled: bool,
}

#[cfg(test)]
#[path = "breakpoint_tests.rs"]
mod tests;
