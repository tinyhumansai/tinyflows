//! What happens after a run: judge it, record it, score it, decide.
//!
//! The closing half of the loop. Intake decided *how* to attempt the goal and
//! the engine carried it out; this reads what came back and turns it into the
//! two things that outlive the attempt — a ledger row, and a score against the
//! workflow that ran.
//!
//! The order matters and is not obvious. **Recording happens whatever the
//! verdict**, before any decision about retrying. A run that failed and was not
//! written down is a run the next attempt will repeat, so the ledger write is
//! not conditional on success — it is most valuable when the news is bad.

mod consolidate;
mod judge;
mod keep;
mod repair;
mod resume;

pub use consolidate::consolidate;
pub use judge::{Evidence, judge};
pub use keep::{Kept, keep};
pub use repair::{Variant, graph_is_suspect, repair};
pub use resume::may_continue;

use crate::contracts::{Approach, Budget, Goal, Verdict};
use crate::execute::StepRecord;
use crate::intake::Result;
use crate::ledger::{Episode, EpisodeStatus, Ledger, LedgerRow};
use std::collections::HashMap;
use tinyflows::caps::Capabilities;
use tinyflows::model::{NodeKind, WorkflowGraph};

/// The workflows this graph called, and whether each one's step succeeded.
///
/// Read off the graph rather than reported by the runner: which nodes are
/// calls is a property of the plan, so a host implementing [`Runner`] does not
/// have to know this scoring exists to participate in it.
///
/// A node with no step record never ran — the graph stopped short of it — and
/// is credited with nothing at all, not even `applied`. An id written as an
/// `=`-expression is skipped: it names a workflow only once the run resolves
/// it, and scoring the literal text would move counters on a workflow that
/// does not exist.
///
/// **One entry per activation, not per node.** A node inside a loop produces
/// one [`StepRecord`] per iteration, so the walk is over the *records* with the
/// graph as a lookup, not over the nodes taking the first record each. Reading
/// only the first would drop every later call and — worse — let an early
/// success hide a later error, crediting a workflow for a run that failed.
///
/// [`Runner`]: crate::execute::Runner
fn called_workflows(graph: &WorkflowGraph, steps: &[StepRecord]) -> Vec<(String, bool)> {
    let calls: HashMap<&str, &str> = graph
        .nodes
        .iter()
        .filter(|node| node.kind == NodeKind::SubWorkflow)
        .filter_map(|node| {
            let called = node
                .config
                .get("workflow_id")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|id| !id.is_empty() && !id.starts_with('='))?;
            Some((node.id.as_str(), called))
        })
        .collect();

    steps
        .iter()
        .filter_map(|step| {
            let called = calls.get(step.node_id.as_str())?;
            Some((
                (*called).to_string(),
                step.status == crate::execute::StepOutcome::Success,
            ))
        })
        .collect()
}

/// What the loop should do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Next {
    /// The goal was met.
    Done,
    /// Attempt it again. The planner will see the exclusion list this closing
    /// pass just added to.
    Retry,
    /// Stop without success: the blocker is terminal, or the budget is spent,
    /// or the run stopped advancing. The reason is worth keeping because
    /// "stood down" and "failed" read very differently to whoever asked.
    StandDown(String),
}

/// One finished attempt, closed out.
#[derive(Debug, Clone)]
pub struct Closed {
    /// What the judge concluded.
    pub verdict: Verdict,
    /// The ledger row this attempt left behind.
    pub row_id: String,
    /// What to do next.
    pub next: Next,
    /// Consecutive non-advancing attempts, to carry into the next pass.
    pub stalled: u32,
}

/// Judge a finished run, record it, score it, and say what to do next.
///
/// Takes the whole [`crate::execute::Ran`] rather than just its
/// [`Evidence`]: the cost and the per-node transcript are on it, both were
/// being measured and then dropped here, and a signature that cannot see them
/// is a signature that will drop them again.
///
/// The stall count is **read from and written back to the episode record**,
/// not threaded by the caller. It used to be a parameter, on the reasoning that
/// two episodes sharing one closing layer must not share a counter — true, but
/// the fix was keying it by episode, not making the caller hold it. A counter
/// that lives only in the caller's memory is a counter a deploy loses, and an
/// episode whose stall count silently resets to zero will keep retrying an
/// approach that stopped working four attempts ago.
///
/// The episode record is created here when it does not exist, so
/// [`Ledger::save_episode`] is optional for a caller that only wants the loop.
///
/// # Errors
/// When inference fails, or the ledger cannot be read or written.
#[allow(clippy::too_many_arguments)]
pub async fn close(
    goal: &Goal,
    episode: &str,
    attempt: u32,
    approach: &Approach,
    graph: &WorkflowGraph,
    ran: &crate::execute::Ran,
    budget: &Budget,
    ledger: &dyn Ledger,
    caps: &Capabilities,
    conn: Option<&str>,
    now: &str,
) -> Result<Closed> {
    let verdict = judge(goal, &ran.evidence(), caps, conn).await?;
    let mut record = ledger.episode(episode).await?.unwrap_or(Episode {
        id: episode.to_string(),
        goal: goal.clone(),
        scope_key: None,
        status: EpisodeStatus::Running,
        attempt: 0,
        stalled: 0,
        started_at: now.to_string(),
        updated_at: now.to_string(),
    });
    let stalled = record.stalled;

    // Recorded before anything is decided, and whatever the verdict. A failed
    // attempt nobody wrote down is one the next attempt repeats.
    let workflow_id = match approach {
        // The id that ran, which for a repaired graph is the variant's own —
        // scoring its parent instead would leave the two indistinguishable and
        // the promotion gate with nothing to compare.
        Approach::Selected { workflow_id, .. } => Some(workflow_id.clone()),
        // Neither has a stored procedure behind it, so there is no counter to
        // move. An errand additionally has nothing to *become* one: the row it
        // leaves is the whole record of it.
        Approach::Authored { .. } | Approach::Errand { .. } => None,
    };
    let row_id = ledger
        .append(&LedgerRow {
            id: String::new(),
            episode: episode.to_string(),
            attempt,
            approach_sig: approach.signature(),
            approach_desc: why(approach),
            workflow_id: workflow_id.clone(),
            outcome: outcome_line(&verdict),
            cause: verdict.gap.clone(),
            // What the runner measured. It was on the wire and on `Ran` all
            // along; writing zero here made every row claim the attempt was
            // free, which is indistinguishable from a host that does not meter.
            cost_usd: ran.cost_usd,
            at: now.to_string(),
            satisfied: verdict.satisfied,
            advanced: verdict.advanced,
        })
        .await?;

    // The per-node record behind the row. Best-effort: the attempt is judged
    // and scored either way, and losing the transcript costs a reader detail
    // rather than costing the loop its result.
    let _ = ledger.save_steps(&row_id, &ran.steps).await;

    // The rung medulla-v2 never had: without this nothing distinguishes a
    // procedure that has worked forty times from one that has never run, and
    // the promotion gate has no evidence to read.
    if let Some(id) = workflow_id {
        ledger.score_workflow(&id, verdict.satisfied).await?;
    }

    // And the workflows this attempt *called*. Without this a workflow only
    // ever used as a component stays Unproven forever: the chooser distrusts
    // it, the promotion gate cannot see it, and composition becomes a place
    // procedures go to stop earning a reputation.
    //
    // Same standard a selection is held to — it ran, and the attempt was
    // judged satisfied — with one addition a selection does not need. A
    // selected workflow IS the attempt, so the attempt's verdict is its
    // verdict. A called one is a part, so its own step must also have
    // succeeded: a child that errored inside a plan that recovered around it
    // has been exercised, not vindicated, and reads `applied` without
    // `helped`.
    //
    // Weaker evidence than a selection's, and worth knowing it: nothing here
    // judges the child's *output*, so a child that ran cleanly and
    // contributed nothing to an episode satisfied by its siblings is credited
    // anyway. Establishing more would cost a judge call per child, which is
    // the thing the loop's economics are built to avoid.
    for (called, worked) in called_workflows(graph, &ran.steps) {
        ledger
            .score_workflow(&called, worked && verdict.satisfied)
            .await?;
    }

    let stalled = if verdict.satisfied || verdict.advanced {
        0
    } else {
        stalled + 1
    };
    let next = decide_next(&verdict, attempt, stalled, budget);

    // Written after the row and the score, so a checkpoint never claims an
    // attempt the ledger has no record of.
    record.attempt = attempt;
    record.stalled = stalled;
    record.updated_at = now.to_string();
    record.status = match &next {
        Next::Done => EpisodeStatus::Satisfied,
        Next::Retry => EpisodeStatus::Running,
        Next::StandDown(reason) => EpisodeStatus::StoodDown(reason.clone()),
    };
    ledger.save_episode(&record).await?;

    Ok(Closed {
        verdict,
        row_id,
        next,
        stalled,
    })
}

fn decide_next(verdict: &Verdict, attempt: u32, stalled: u32, budget: &Budget) -> Next {
    if verdict.satisfied {
        return Next::Done;
    }
    if verdict.should_retry(attempt, stalled, budget) {
        return Next::Retry;
    }
    // Each reason is worth distinguishing: a terminal blocker is the goal's
    // fault, a spent budget is ours, and a stall is the approach running out
    // of ideas. Collapsing them to "failed" loses the only thing a reader can
    // act on.
    Next::StandDown(if !verdict.blocker.continuable() {
        format!("{:?} — {}", verdict.blocker, verdict.gap)
    } else if budget.exhausted(attempt) {
        format!("out of attempts after {attempt}")
    } else {
        format!("{stalled} attempts in a row made no progress")
    })
}

fn why(approach: &Approach) -> String {
    match approach {
        Approach::Selected { why, .. }
        | Approach::Authored { why, .. }
        | Approach::Errand { why } => why.clone(),
    }
}

fn outcome_line(verdict: &Verdict) -> String {
    if verdict.satisfied {
        "satisfied".to_string()
    } else if verdict.gap.is_empty() {
        format!("{:?}", verdict.blocker)
    } else {
        verdict.gap.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::Blocker;

    fn verdict(satisfied: bool, blocker: Blocker, advanced: bool) -> Verdict {
        Verdict {
            satisfied,
            blocker,
            gap: "something is missing".into(),
            attributed_to: String::new(),
            evidence: String::new(),
            advanced,
        }
    }

    #[test]
    fn a_satisfied_verdict_is_done() {
        let next = decide_next(
            &verdict(true, Blocker::None, true),
            1,
            0,
            &Budget::default(),
        );
        assert_eq!(next, Next::Done);
    }

    #[test]
    fn an_ordinary_shortfall_retries() {
        let next = decide_next(
            &verdict(false, Blocker::GoalNotMet, true),
            1,
            0,
            &Budget::default(),
        );
        assert_eq!(next, Next::Retry);
    }

    #[test]
    fn a_terminal_blocker_stands_down_naming_itself() {
        let next = decide_next(
            &verdict(false, Blocker::NeedsInput, true),
            1,
            0,
            &Budget::default(),
        );
        match next {
            Next::StandDown(reason) => assert!(reason.contains("NeedsInput"), "{reason}"),
            other => panic!("expected a stand-down, got {other:?}"),
        }
    }

    #[test]
    fn a_spent_budget_says_so_rather_than_blaming_the_approach() {
        let next = decide_next(
            &verdict(false, Blocker::GoalNotMet, true),
            12,
            0,
            &Budget::default(),
        );
        match next {
            Next::StandDown(reason) => assert!(reason.contains("out of attempts"), "{reason}"),
            other => panic!("expected a stand-down, got {other:?}"),
        }
    }

    #[test]
    fn a_stall_says_so_rather_than_blaming_the_budget() {
        let next = decide_next(
            &verdict(false, Blocker::GoalNotMet, false),
            5,
            2,
            &Budget::default(),
        );
        match next {
            Next::StandDown(reason) => assert!(reason.contains("no progress"), "{reason}"),
            other => panic!("expected a stand-down, got {other:?}"),
        }
    }

    #[test]
    fn an_advancing_attempt_clears_the_stall_count() {
        // The whole reason `advanced` exists: a run converging over five
        // attempts must not accumulate a stall from the two that looked flat.
        let budget = Budget::default();
        assert_eq!(
            decide_next(&verdict(false, Blocker::GoalNotMet, true), 9, 0, &budget),
            Next::Retry
        );
    }

    #[test]
    fn the_ledger_row_records_a_failure_in_its_own_words() {
        let v = verdict(false, Blocker::GoalNotMet, true);
        assert_eq!(outcome_line(&v), "something is missing");
        assert_eq!(
            outcome_line(&verdict(true, Blocker::None, true)),
            "satisfied"
        );
    }

    #[test]
    fn a_blockers_name_is_the_outcome_when_the_judge_gave_no_gap() {
        let mut v = verdict(false, Blocker::MissingEvidence, false);
        v.gap = String::new();
        assert_eq!(outcome_line(&v), "MissingEvidence");
    }
}
