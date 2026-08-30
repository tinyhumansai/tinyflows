//! Holding the pieces together, and driving an episode to an answer.
//!
//! Everything below this module is a free function taking seven or eight
//! arguments, which is right for a library and wearing to call. This bundles
//! them.
//!
//! # What is an instance, and what is a goal run
//!
//! These are different lifetimes and putting them in one object is the mistake
//! worth naming.
//!
//! A [`Loop`] is **per tenant**, long-lived, and holds only configuration and
//! adapters: a scoped ledger, a workflow store, capabilities, host facts, a
//! runner, a budget. Building one costs a database pool and an HTTP client, so
//! it is built once and shared.
//!
//! A **goal run is an episode id**, not an object. Its state — the goal, the
//! attempt number, the stall count, whether it finished — lives in the
//! [`Episode`] record in the ledger. Nothing about it is held here.
//!
//! That split is what makes two things true at once. Many goal runs share one
//! `Loop`, concurrently, because the `Loop` holds nothing per-episode. And an
//! episode survives the process running it: kill this one mid-run and
//! [`Ledger::episodes`] on the next boot hands back everything that was in
//! flight, each resumable from its own record by id.
//!
//! Had the instance *been* the goal run, both would be false — the config would
//! be rebuilt per goal, and a deploy would lose every episode's counters while
//! leaving its rows behind to look like progress.

use std::sync::Arc;

use crate::contracts::ResumePoint;
use crate::intake::Attempt;
use tinyflows::caps::Capabilities;
use tinyflows::store::WorkflowStore;

use crate::closing::{self, Closed, Next, graph_is_suspect};
use crate::contracts::{Approach, Budget, Goal, Verdict};
use crate::execute::Runner;
use crate::host::HostFacts;
use crate::intake::{Result, decide};
use crate::ledger::{Episode, EpisodeStatus, Ledger, Lesson, Page};

/// Where timestamps come from.
///
/// A seam rather than a dependency: the crate has no clock of its own, so a
/// frozen one drives tests and the host brings whatever it already uses. Every
/// stored time is caller-supplied for the same reason.
pub trait Clock: Send + Sync {
    /// The current time, RFC 3339.
    fn now(&self) -> String;
}

/// One tenant's configuration and adapters.
///
/// Cheap to hold, expensive to build. See the module note on why this is not
/// one per goal run.
pub struct Loop<'a> {
    /// Scoped to this tenant — see [`Ledger::scope`].
    pub ledger: &'a dyn Ledger,
    /// Where workflows are read and variants written.
    pub store: &'a Arc<dyn WorkflowStore>,
    /// Inference. The `tier` on each request says which job is asking.
    pub caps: &'a Capabilities,
    /// What the machine that runs graphs permits.
    pub facts: &'a HostFacts,
    /// In-process or relayed; the loop cannot tell.
    pub runner: &'a dyn Runner,
    /// Where timestamps come from.
    pub clock: &'a dyn Clock,
    /// How hard to try.
    pub budget: Budget,
    /// Opaque credential reference, passed to inference untouched.
    pub conn: Option<&'a str>,
}

/// How an episode ended.
#[derive(Debug, Clone)]
pub struct Finished {
    /// Satisfied, or stood down with a reason.
    pub status: EpisodeStatus,
    /// How many attempts it took.
    pub attempts: u32,
    /// What the judge said about the last one.
    pub verdict: Verdict,
    /// What was worth remembering. Usually nothing.
    pub lessons: Vec<Lesson>,
}

impl Loop<'_> {
    /// Begin an episode, or return the one already under way.
    ///
    /// Idempotent, so a service that retries a create does not restart a goal
    /// that is four attempts in.
    ///
    /// # Errors
    /// When the ledger cannot be read or written.
    pub async fn start(&self, episode: &str, goal: &Goal) -> Result<Episode> {
        if let Some(existing) = self.ledger.episode(episode).await? {
            return Ok(existing);
        }
        let now = self.clock.now();
        let record = Episode {
            id: episode.to_string(),
            goal: goal.clone(),
            scope_key: None,
            status: EpisodeStatus::Running,
            attempt: 0,
            stalled: 0,
            started_at: now.clone(),
            updated_at: now,
        };
        self.ledger.save_episode(&record).await?;
        Ok(record)
    }

    /// One pass: decide, run, judge, record — and repair the graph if that is
    /// what fell short.
    ///
    /// **One episode, one attempt at a time.** Concurrency is across episodes —
    /// distinct ids — never within one: two concurrent `attempt` calls for the
    /// same episode id would read the same attempt number, append two rows
    /// under it, and race the checkpoint write. Nothing here serializes that,
    /// because the natural caller — [`run`](Self::run), or a worker owning an
    /// episode — is already sequential; a host that parallelizes within one
    /// episode owns the lock.
    ///
    /// The attempt number comes from the episode record rather than the caller,
    /// so a process that picks up an episode it did not start continues its
    /// numbering instead of restarting at one.
    ///
    /// # Errors
    /// When intake cannot decide, or the ledger cannot be read or written.
    /// Running never errors — see [`crate::execute`].
    pub async fn attempt(&self, episode: &str, goal: &Goal) -> Result<Closed> {
        Ok(self.attempt_continuing(episode, goal, None).await?.0)
    }

    /// [`attempt`](Self::attempt), given the chance to continue a run the
    /// previous one left unfinished.
    ///
    /// Returns the close *and* the continuation the next attempt may use —
    /// `Some` only when this attempt's repair left the failed node's whole
    /// upstream untouched, and only for the variant that repair produced.
    /// [`run`](Self::run) threads that value; a host driving attempts itself
    /// threads it the same way, or passes `None` and pays for the prefix
    /// again, which is what every attempt did before continuing existed.
    ///
    /// The continuation is carried rather than stored because it is worth
    /// exactly one attempt: it names a boundary in a checkpointer whose thread
    /// the *next* run will write over, and a stale one would re-enter a graph
    /// on some other run's prefix.
    ///
    /// # Errors
    /// As [`attempt`](Self::attempt).
    pub async fn attempt_continuing(
        &self,
        episode: &str,
        goal: &Goal,
        from: Option<ResumePoint>,
    ) -> Result<(Closed, Option<ResumePoint>)> {
        let record = self.start(episode, goal).await?;
        let attempt = record.attempt + 1;

        let planned = decide(
            goal,
            episode,
            self.store.as_ref(),
            self.ledger,
            self.facts,
            self.caps,
            self.conn,
        )
        .await?;

        // The continuation only applies to the workflow the repair produced.
        // The chooser is free to pick something else entirely — a different
        // variant, the parent, nothing at all — and a prefix committed by one
        // graph is not a prefix for another.
        let planned = match (from, &planned.approach) {
            (Some(point), Approach::Selected { workflow_id, .. })
                if workflow_id == &point.workflow =>
            {
                Attempt {
                    resume: Some(point),
                    ..planned
                }
            }
            _ => planned,
        };

        let ran = self.runner.run(&planned).await;
        let closed = closing::close(
            goal,
            episode,
            attempt,
            &planned.approach,
            &planned.graph,
            &ran,
            &self.budget,
            self.ledger,
            self.caps,
            self.conn,
            &self.clock.now(),
        )
        .await?;

        // The other half of the knowledge ladder's scoring. `close` moves the
        // workflow's counters; nothing was moving a lesson's, so a lesson that
        // was read forty times and never helped looked exactly like one written
        // this morning.
        for lesson in &planned.lessons_shown {
            let _ = self
                .ledger
                .score_lesson(lesson, closed.verdict.satisfied)
                .await;
        }

        let next = if closed.verdict.satisfied {
            self.keep_if_it_generalises(goal, &planned).await;
            None
        } else {
            self.repair_if_the_graph_is_at_fault(goal, &closed, &planned, &ran)
                .await
        };
        Ok((closed, next))
    }

    /// Drive an episode until it is satisfied or stands down.
    ///
    /// Terminates without a bound of its own: `close` returns
    /// [`Next::StandDown`] once the budget is spent or the run stops advancing,
    /// so the exit condition lives in one place rather than two that can
    /// disagree.
    ///
    /// # Errors
    /// As [`attempt`](Self::attempt).
    pub async fn run(&self, episode: &str, goal: &Goal) -> Result<Finished> {
        let mut carry: Option<ResumePoint> = None;
        loop {
            let (closed, next) = self.attempt_continuing(episode, goal, carry.take()).await?;
            carry = next;
            let status = match &closed.next {
                Next::Retry => continue,
                Next::Done => EpisodeStatus::Satisfied,
                Next::StandDown(reason) => EpisodeStatus::StoodDown(reason.clone()),
            };

            // Once per episode, not per attempt: what generalises is visible
            // from the whole trail and not from any one row of it.
            let lessons = closing::consolidate(
                goal,
                episode,
                closed.verdict.satisfied,
                self.ledger,
                self.caps,
                self.conn,
            )
            .await;

            let attempts = self
                .ledger
                .episode(episode)
                .await?
                .map_or(0, |record| record.attempt);
            return Ok(Finished {
                status,
                attempts,
                verdict: closed.verdict,
                lessons,
            });
        }
    }

    /// Every episode of this tenant's that was still running.
    ///
    /// The boot recovery list. Without it a deploy abandons whatever was in
    /// flight: the rows stay, nothing looks at them again, and the goal is
    /// never answered.
    ///
    /// # Errors
    /// When the ledger cannot be read.
    pub async fn unfinished(&self) -> Result<Vec<Episode>> {
        Ok(self.ledger.episodes(true, Page::ALL).await?)
    }

    /// Keep a graph that was authored for this goal and achieved it.
    ///
    /// Only an authored one: a selected workflow is already stored, and a
    /// repaired variant was stored when it was proposed.
    ///
    /// Best-effort and silent on failure, like the other two post-outcome
    /// passes. The goal is met either way; failing to file the procedure costs
    /// the next episode an authoring call, not this one its result.
    async fn keep_if_it_generalises(&self, goal: &Goal, planned: &crate::intake::Attempt) {
        if !matches!(planned.approach, Approach::Authored { .. }) {
            return;
        }
        let kept = closing::keep(
            goal,
            &planned.graph,
            &planned.inputs,
            self.store,
            self.caps,
            self.conn,
        )
        .await;

        // Scored on the way in, from the run that earned it. A procedure
        // entering the catalogue at 0/0 is indistinguishable from one nobody
        // has ever run, and the evidence that it works is the episode that just
        // finished.
        if let Ok(Some(kept)) = kept {
            let _ = self.ledger.score_workflow(&kept.record.id, true).await;
        }
    }

    /// Propose a variant when the diagnosis says the graph was the problem.
    ///
    /// Best-effort and deliberately silent on failure. It runs after the
    /// outcome is already recorded, so a refused batch or a provider hiccup
    /// must not turn a judged attempt into a failed one — the same reasoning as
    /// [`crate::closing::consolidate`].
    async fn repair_if_the_graph_is_at_fault(
        &self,
        goal: &Goal,
        closed: &Closed,
        planned: &Attempt,
        ran: &crate::execute::Ran,
    ) -> Option<ResumePoint> {
        if closed.verdict.satisfied {
            return None;
        }
        // Whatever ran is the parent of the next repair — including a variant,
        // which makes a second generation. `Ledger::lineage` walks to the root,
        // so a grandchild is still compared inside one family.
        let parent = match &planned.approach {
            Approach::Selected { workflow_id, .. } => workflow_id,
            // Nothing to repair: an authored graph was written for this goal
            // and the next attempt writes another, seeing why this one fell
            // short. A variant of a one-off is a stored procedure nobody asked
            // for.
            //
            // An errand is the same argument at its limit — there is no
            // procedure at all, only a turn that did not land. What it needs is
            // a plan, and `decide` will write one: the errand is spent, so the
            // next attempt cannot choose it again.
            Approach::Authored { .. } | Approach::Errand { .. } => return None,
        };
        let evidence = ran.evidence();
        if !graph_is_suspect(&closed.verdict, &evidence) {
            return None;
        }
        let variant = closing::repair(
            goal,
            &closed.verdict,
            &evidence,
            parent,
            self.store,
            self.ledger,
            self.caps,
            self.conn,
        )
        .await
        .ok()
        .flatten()?;

        // The variant exists either way. What the gate decides is only whether
        // the next attempt may skip the prefix — a `false` costs the work
        // again, which is what every attempt cost before this existed.
        let stopped = ran.resume.as_ref()?;
        closing::may_continue(
            &planned.graph,
            &variant.record.graph,
            &stopped.failed_node,
            &variant.ops,
        )
        .then(|| ResumePoint {
            workflow: variant.record.id.clone(),
            ..stopped.clone()
        })
    }
}

#[cfg(test)]
#[path = "driver_tests.rs"]
mod tests;
