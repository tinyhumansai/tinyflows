//! The control arm: a loop that still retries, and still records, but is told
//! nothing about episodes other than its own.
//!
//! An experiment needs the two arms to differ in **one** thing. That thing is
//! not "does it retry" — the episode ledger is not what is under test, and an
//! arm that could not retry would lose for the wrong reason. It is whether
//! anything survives *to the next episode*.
//!
//! Which makes the control arm awkward here in a way it is not elsewhere: this
//! loop's ledger is load-bearing **within** an episode. It holds the exclusion
//! list that stops attempt four repeating attempt two, and the stall counter
//! that decides when to stand down. Dropping it would not turn learning off;
//! it would turn the loop into something that cannot plan.
//!
//! So [`Forgetful`] blanks exactly the cross-episode reads and leaves
//! everything else alone:
//!
//! | Read | On | Off |
//! | --- | --- | --- |
//! | `rows` / `episode` / `steps` — this episode | real | real |
//! | `lessons` / `evidence` — other episodes | real | empty |
//! | `workflow_score` — evidence from prior runs | real | 0/0 |
//! | `lineage` / `parent_of` / `children_of` — repair families | real | empty |
//!
//! **Writes still happen.** The control arm consolidates, promotes and scores
//! exactly as the treatment arm does, and then never reads any of it. That is
//! deliberate: if the off arm skipped the writes it would also skip their
//! model calls, and `cost_per_solve` would favour it for a reason that has
//! nothing to do with learning. Isolating recall is the point.
//!
//! **The workflow store is the caller's half.** Learned graphs live in the
//! store, not the ledger, so an off arm must also be handed a store that is
//! not carried between episodes — otherwise the second episode can select what
//! the first one kept, and the arms differ in less than they appear to. There
//! is no wrapper for that because there is nothing to wrap: give the arm a
//! fresh store per episode.

use async_trait::async_trait;

use crate::execute::StepRecord;
use crate::ledger::{Episode, Ledger, LedgerRow, Lesson, LessonKind, Page, Result, Score};

/// A [`Ledger`] that answers every cross-episode question with nothing.
///
/// Wraps a real one so the control arm's episodes are still recorded — the
/// experiment reads those rows to compute cost and workflow use, so an arm
/// that wrote nowhere could not be measured.
pub struct Forgetful<L> {
    inner: L,
}

impl<L> Forgetful<L> {
    /// Wrap `inner`, keeping its within-episode behaviour and blanking the
    /// rest.
    pub const fn new(inner: L) -> Self {
        Self { inner }
    }

    /// The ledger underneath, for reading what the arm recorded after the fact.
    pub const fn inner(&self) -> &L {
        &self.inner
    }
}

#[async_trait]
impl<L: Ledger> Ledger for Forgetful<L> {
    async fn append(&self, row: &LedgerRow) -> Result<String> {
        self.inner.append(row).await
    }

    async fn rows(&self, episode: &str) -> Result<Vec<LedgerRow>> {
        // This episode's own attempts. Blanking these would stop the loop
        // planning, not stop it learning.
        self.inner.rows(episode).await
    }

    async fn promote(&self, lesson: &Lesson, cites: &[String]) -> Result<String> {
        // Written and never read — see the module doc on why the control arm
        // still pays for consolidation.
        self.inner.promote(lesson, cites).await
    }

    async fn lessons(&self, _kind: Option<LessonKind>) -> Result<Vec<Lesson>> {
        Ok(Vec::new())
    }

    async fn evidence(&self, _lesson_id: &str) -> Result<Vec<LedgerRow>> {
        Ok(Vec::new())
    }

    async fn score_lesson(&self, lesson_id: &str, helped: bool) -> Result<()> {
        self.inner.score_lesson(lesson_id, helped).await
    }

    async fn score_workflow(&self, workflow_id: &str, helped: bool) -> Result<()> {
        self.inner.score_workflow(workflow_id, helped).await
    }

    async fn workflow_score(&self, _workflow_id: &str) -> Result<Score> {
        // Not "never run" as a lie — in this arm it genuinely has no record
        // the planner is entitled to see.
        Ok(Score::default())
    }

    async fn link_variant(&self, parent: &str, variant: &str) -> Result<()> {
        self.inner.link_variant(parent, variant).await
    }

    async fn parent_of(&self, _id: &str) -> Result<Option<String>> {
        Ok(None)
    }

    async fn children_of(&self, _id: &str) -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    async fn save_episode(&self, episode: &Episode) -> Result<()> {
        self.inner.save_episode(episode).await
    }

    async fn episode(&self, id: &str) -> Result<Option<Episode>> {
        self.inner.episode(id).await
    }

    async fn episodes(&self, running_only: bool, page: Page) -> Result<Vec<Episode>> {
        self.inner.episodes(running_only, page).await
    }

    async fn save_steps(&self, row_id: &str, steps: &[StepRecord]) -> Result<()> {
        self.inner.save_steps(row_id, steps).await
    }

    async fn steps(&self, row_id: &str) -> Result<Vec<StepRecord>> {
        self.inner.steps(row_id).await
    }
}
