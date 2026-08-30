//! The types the loop turns on.
//!
//! Ported from medulla-v2, where each of them was arrived at by a failure
//! rather than by design. The comments record which failure, because the shape
//! is not obvious from the type and a later reader will otherwise simplify one
//! of them back into the thing that broke.
//!
//! What is deliberately absent: anything shaped like a plan. A plan here is a
//! `WorkflowGraph` — the engine's own type — and nothing in this module
//! duplicates it. The loop decides *which* graph; the engine runs it.

use serde::{Deserialize, Serialize};

/// Why a run did not satisfy its goal.
///
/// A fixed vocabulary rather than free text, because the loop branches on it:
/// two of these mean "try again", two mean "stop", and one means "ask". Free
/// text cannot be branched on, and a model asked for a category invents a new
/// one every third call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Blocker {
    /// Satisfied. No blocker.
    None,
    /// Something was produced but the evidence does not show it working.
    /// Continuable: another attempt can verify it.
    Unverified,
    /// The goal was not met and the attempt made a real try at it.
    /// Continuable: this is the ordinary retry case.
    GoalNotMet,
    /// Nothing was produced and there is nothing to judge. Terminal, because a
    /// retry with the same inputs produces the same nothing.
    MissingEvidence,
    /// A person has to answer something before this can continue.
    NeedsInput,
    /// Waiting on something outside the system — a deploy, a review, a rate
    /// limit. Retrying now is not the same as retrying later.
    ExternalWait,
}

impl Blocker {
    /// Whether another attempt could plausibly do better.
    #[must_use]
    pub fn continuable(self) -> bool {
        matches!(self, Self::Unverified | Self::GoalNotMet)
    }

    /// Reads a model's answer, coercing anything unrecognised to the safest
    /// continuable value.
    ///
    /// A misspelling used to end runs: `goal_not_meet` fell through to a
    /// terminal default and killed a run at attempt 3 of 12. The model is not
    /// going to stop misspelling, so the boundary absorbs it.
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "none" | "" => Self::None,
            "unverified" => Self::Unverified,
            "missing_evidence" => Self::MissingEvidence,
            "needs_input" => Self::NeedsInput,
            "external_wait" => Self::ExternalWait,
            _ => Self::GoalNotMet,
        }
    }
}

/// What the judge produced after a run.
///
/// Carries no plan-shaped field, on purpose. The judge runs context-poor — goal,
/// outcome and evidence only — so it can diagnose but cannot sensibly propose
/// what to do next: it does not know what has already been ruled out. Deciding
/// that is the planner's job, and the planner has the ledger.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Verdict {
    /// Whether the goal was met.
    pub satisfied: bool,
    /// Why not, when it was not.
    pub blocker: Blocker,
    /// What is still missing, in one sentence, for the next plan to read.
    #[serde(default)]
    pub gap: String,
    /// Which node or step fell short, when the judge can tell.
    #[serde(default)]
    pub attributed_to: String,
    /// What the judge actually looked at. Recorded so a wrong verdict can be
    /// argued with later.
    #[serde(default)]
    pub evidence: String,
    /// Did this attempt move the goal closer than it was before it ran?
    ///
    /// The decision to try again used to be a counter, which cannot tell a run
    /// that is converging from one that is spinning — two live runs were killed
    /// at 7 of 10 and climbing, while a third thrashed 10 → 2 → 1 and only the
    /// counter stopped it. All three reported `goal_not_met`.
    #[serde(default = "yes")]
    pub advanced: bool,
}

fn yes() -> bool {
    true
}

impl Verdict {
    /// Whether the loop should attempt again, given how many attempts have run.
    ///
    /// Three gates, in order. `min_attempts` comes first because early attempts
    /// routinely look flat while a run is still orienting — the first often
    /// only establishes what it is dealing with — so a stall call on attempt one
    /// ends runs that had not started.
    #[must_use]
    pub fn should_retry(&self, spent: u32, stalled: u32, budget: &Budget) -> bool {
        if self.satisfied || !self.blocker.continuable() {
            return false;
        }
        if budget.exhausted(spent) {
            return false;
        }
        if spent < budget.min_attempts {
            return true;
        }
        stalled < budget.stall_limit
    }
}

/// What one episode may spend.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Budget {
    /// A backstop, not the stop rule. A run normally ends because the judge says
    /// two attempts in a row went nowhere.
    pub attempts: u32,
    /// Attempts before the stall rule may end a run at all.
    pub min_attempts: u32,
    /// Consecutive non-advancing attempts that end a run.
    pub stall_limit: u32,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            attempts: 12,
            min_attempts: 3,
            stall_limit: 2,
        }
    }
}

impl Budget {
    /// Whether the attempt ceiling has been reached.
    #[must_use]
    pub fn exhausted(&self, spent: u32) -> bool {
        spent >= self.attempts
    }
}

/// Which job the loop is asking a model to do.
///
/// Emitted on every inference request as `tier`, and that is the whole of it —
/// the crate names the **job**, never a model, a vendor or a URL, because only
/// the host knows which of those a job maps to. That is the host-agnostic rule
/// the engine sits on and this crate keeps.
///
/// It is what makes a tier sweep a config change rather than a code change:
/// judging is the expensive opinion and selecting is a cheap one, and without a
/// name on the request a host cannot route them differently.
///
/// Called `tier` rather than `role` on the wire because a chat request already
/// has `role` on every message, and two meanings of one key in one payload is a
/// bug waiting for a hurried reader.
///
/// Six, not medulla-v2's three: a host can map several tiers to one model in a
/// line of config, and cannot split one tier into two at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// Does a stored workflow already do this? Cheap; a short list and a yes/no.
    Select,
    /// Write a graph. The hardest reasoning the loop does.
    Author,
    /// Did the run achieve the goal? The opinion worth paying for — a judge
    /// that says yes wrongly ends the episode.
    Judge,
    /// What is this episode worth remembering? Off the critical path, and
    /// nothing downstream blocks on it.
    Consolidate,
    /// Repair a graph that fell short. Structured editing against a diagnosis.
    Repair,
    /// Name a graph that worked, so a later goal can find it. Prose only — the
    /// graph is already fixed.
    Generalise,
}

impl Tier {
    /// The name that goes on the wire.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Select => "select",
            Self::Author => "author",
            Self::Judge => "judge",
            Self::Consolidate => "consolidate",
            Self::Repair => "repair",
            Self::Generalise => "generalise",
        }
    }
}

/// What the user asked for, and what would prove it done.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Goal {
    /// The prompt, verbatim. Never paraphrased on its way anywhere: a detail
    /// misremembered in a restatement becomes the only version an agent sees.
    pub text: String,
    /// What would show it satisfied. Empty when the user gave no criterion and
    /// the judge has to infer one from the goal.
    #[serde(default)]
    pub success_criteria: String,
}

impl Goal {
    /// A goal with no stated success criterion; the judge infers one.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            success_criteria: String::new(),
        }
    }
}

/// How the loop decided to attempt a goal this time.
///
/// Two, and the second is what makes this a loop rather than a router: when no
/// stored procedure fits, one is written.
///
/// There is deliberately no `Variant` arm. A repaired graph is saved to the
/// store as a workflow in its own right, so the attempt that runs it is a
/// [`Selected`](Self::Selected) of *that* id — which is what the score has to
/// land on. A third arm naming the parent would score the parent for a run the
/// variant did, leaving the two indistinguishable and the promotion gate with
/// nothing to compare. What makes it a variant is the lineage in the ledger,
/// not the shape of this enum.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Approach {
    /// A stored workflow matched. The common case once anything has been
    /// learned, and the cheap one: no authoring call.
    Selected {
        /// The stored workflow that matched.
        workflow_id: String,
        /// Why it was chosen, for the ledger row.
        why: String,
    },
    /// Nothing fitted, so a graph was written for this goal.
    Authored {
        /// Why nothing stored fitted.
        why: String,
        /// A digest of the graph that was written.
        ///
        /// The exclusion list is built from [`signature`](Self::signature), so
        /// without this every authoring attempt in an episode signs as the same
        /// string, `tried()` folds them to one entry, and attempt four can
        /// re-author attempt two's graph word for word with nothing to notice.
        /// The digest is what makes two authored attempts distinguishable —
        /// and makes an identical re-author visible as the repeat it is.
        fingerprint: String,
    },
    /// One turn of work with no procedure in it.
    ///
    /// The third answer to "does anything stored do this", and the one that
    /// says the question was wrong: some goals are not a procedure at all.
    /// "What is the disk usage of this directory" has nothing in it worth
    /// writing down, and putting it through authoring pays a large planning
    /// call to produce a one-step graph, then files that graph where it dilutes
    /// every later selection.
    ///
    /// It is deliberately the *narrowest* of the three. An errand is judged
    /// like any other attempt and can fail; what it cannot do is leave anything
    /// behind. Nothing is kept ([`crate::closing::keep`] takes only authored
    /// graphs), nothing is repaired (there is no procedure to vary), and the
    /// signature is a constant so a second errand inside one episode is visibly
    /// a repeat — if one turn did not do it, the goal was not an errand.
    Errand {
        /// Why no stored workflow was needed and none is worth writing.
        why: String,
    },
}

impl Approach {
    /// The label a ledger row is keyed on, and the exclusion list is built from.
    ///
    /// Names the *kind* of attempt, not the task: a retry told not to repeat
    /// `review_pr_5478` has nothing left to try, while one told not to repeat
    /// `selected:pr-review` can still author.
    #[must_use]
    pub fn signature(&self) -> String {
        match self {
            Self::Selected { workflow_id, .. } => format!("selected:{workflow_id}"),
            Self::Authored { fingerprint, .. } => format!("authored:{fingerprint}"),
            // No discriminator, on purpose. Two authored attempts are told
            // apart by their fingerprints because the second may be a genuinely
            // different graph; two errands cannot be, because an errand carries
            // no plan to differ in. Signing them the same is what makes the
            // second one read as the repeat it is — and what lets `decide` stop
            // offering the option once it has been spent.
            Self::Errand { .. } => "errand".to_string(),
        }
    }
}

#[cfg(test)]
#[path = "contracts_tests.rs"]
mod tests;

/// Where a failed run stopped, so a later attempt can carry on from it.
///
/// A run that failed at a node leaves its prefix committed in the engine's
/// failure boundary. This is the handle to that: which thread holds it, and
/// which node it stopped at. A [`Runner`](crate::execute::Runner) reports one
/// on [`Ran`](crate::execute::Ran) when the host it runs on keeps
/// checkpoints; the loop hands one back on the next
/// [`Attempt`](crate::intake::Attempt) when — and only when — the repair it
/// made is safe to skip the prefix over
/// ([`may_continue`](crate::closing::may_continue)).
///
/// A host with no checkpointer reports `None` and receives `None`, and every
/// attempt starts at the trigger exactly as it always did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumePoint {
    /// The engine thread the failed run is checkpointed under — for most hosts
    /// the run id they gave it.
    pub thread: String,
    /// The node whose failure ended that run. What a continue re-runs first,
    /// and what the ancestor gate is computed against.
    pub failed_node: String,
    /// The workflow whose graph committed that prefix.
    ///
    /// Carried so the two sides can both check they mean the same run. The
    /// loop only hands a point to an attempt that selected *this* workflow —
    /// the chooser is free to pick something else, and a prefix committed by
    /// one graph is not a prefix for another. A runner may check it again
    /// against the graph it is about to run; a mismatch means continue was
    /// asked for on the wrong thing, and starting from the trigger is the
    /// correct answer.
    pub workflow: String,
}
