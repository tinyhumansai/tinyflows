//! A ledger that forgets.
//!
//! Always compiled — no feature, no driver, no C library — so the crate is
//! usable the moment it is added rather than only after a backend has been
//! chosen. Tests, examples and a first look all want this.
//!
//! # What it is not
//!
//! It is **not the default**, and there is deliberately no `Default` impl on
//! anything that would hand it to a host that did not ask. That is not fussiness
//! about ergonomics; it is the single worst failure this crate could have.
//!
//! Everything else here is built so that a system which appears to be working
//! actually is: a green run with a blank diagnosis means nobody looked, an empty
//! `changed` means nobody checked, an attempt with no ledger row is one the next
//! pass repeats. A ledger silently defaulting to memory is the same shape and
//! worse — the loop runs, the exclusion list works, lessons are written and
//! scored, the tests pass, and every restart throws all of it away. Nobody
//! notices, because the only symptom is that it never gets better.
//!
//! So it is named for what it does, has to be constructed on purpose, and says
//! so in one line at the top. Reach for `super::sqlite` or
//! `super::mongo` the moment learning is supposed to outlive a
//! process.
//!
//! # What it is good for
//!
//! A reference implementation. It passes the same
//! [`conformance`](super::conformance) suite as both real backends, which is
//! worth more than it sounds: it proves the trait is implementable without a
//! database, so a host writing a third backend has a complete, readable example
//! that is checked by the same cases theirs will be.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use super::{Episode, Ledger, LedgerError, LedgerRow, Lesson, LessonKind, Result, Score};

#[derive(Default)]
struct Inner {
    /// Append-only; the index is the sequence, so insertion order survives a
    /// timestamp tie the way both durable backends guarantee. Paired with the
    /// bucket that wrote it, which is a column on the row in both durable
    /// backends and has nowhere to live on `LedgerRow` itself.
    rows: Vec<(String, LedgerRow)>,
    lessons: Vec<Lesson>,
    /// `(lesson_id, row_id)`, deduplicated on insert.
    evidence: Vec<(String, String)>,
    /// Keyed by `(bucket, workflow_id)` — the same composite key sqlite makes a
    /// primary key and mongo matches on.
    scores: HashMap<(String, String), Score>,
    /// `(bucket, variant) -> parent`.
    variants: HashMap<(String, String), String>,
    episodes: Vec<Episode>,
    /// `(bucket, row_id)` to that attempt's steps, in execution order.
    steps: HashMap<(String, String), Vec<crate::execute::StepRecord>>,
}

/// A ledger held in memory, which learns nothing across restarts.
///
/// See the module note before using it for anything but tests.
#[derive(Clone, Default)]
pub struct MemoryLedger {
    inner: Arc<Mutex<Inner>>,
    scope: Option<String>,
}

impl MemoryLedger {
    /// An empty ledger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A handle onto the same store, scoped to one tenant.
    #[must_use]
    pub fn for_tenant(&self, scope: impl Into<String>) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            scope: Some(scope.into()),
        }
    }

    fn bucket(&self) -> String {
        self.scope.clone().unwrap_or_default()
    }

    /// A poisoned lock means a previous caller panicked mid-write. Every write
    /// here is a single statement under the lock, so the data is intact;
    /// refusing every later call would turn one panic into a dead loop — the
    /// same reasoning as the sqlite backend's guard.
    fn guard(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// This bucket plus global, the one read rule everywhere.
    fn visible(&self, scope: Option<&str>) -> bool {
        scope.is_none() || scope == self.scope.as_deref()
    }
}

#[async_trait]
impl Ledger for MemoryLedger {
    fn scope(&self) -> Option<&str> {
        self.scope.as_deref()
    }

    async fn append(&self, row: &LedgerRow) -> Result<String> {
        let mut inner = self.guard();
        let id = format!("ldg_{:08}", inner.rows.len() + 1);
        let bucket = self.bucket();
        inner.rows.push((
            bucket,
            LedgerRow {
                id: id.clone(),
                ..row.clone()
            },
        ));
        Ok(id)
    }

    async fn rows(&self, episode: &str) -> Result<Vec<LedgerRow>> {
        let bucket = self.bucket();
        Ok(self
            .guard()
            .rows
            .iter()
            .filter(|(scope, r)| scope == &bucket && r.episode == episode)
            .map(|(_, r)| r.clone())
            .collect())
    }

    async fn promote(&self, lesson: &Lesson, cites: &[String]) -> Result<String> {
        let mut inner = self.guard();
        let id = format!("les_{:08}", inner.lessons.len() + 1);
        inner.lessons.push(Lesson {
            id: id.clone(),
            // The handle's scope, never the argument's.
            scope_key: self.scope.clone(),
            ..lesson.clone()
        });
        for row_id in cites {
            let edge = (id.clone(), row_id.clone());
            if !inner.evidence.contains(&edge) {
                inner.evidence.push(edge);
            }
        }
        Ok(id)
    }

    async fn lessons(&self, kind: Option<LessonKind>) -> Result<Vec<Lesson>> {
        Ok(self
            .guard()
            .lessons
            .iter()
            .filter(|l| self.visible(l.scope_key.as_deref()))
            .filter(|l| kind.is_none_or(|want| l.kind == want))
            .cloned()
            .collect())
    }

    async fn evidence(&self, lesson_id: &str) -> Result<Vec<LedgerRow>> {
        let inner = self.guard();
        let cited: Vec<&str> = inner
            .evidence
            .iter()
            .filter(|(lesson, _)| lesson == lesson_id)
            .map(|(_, row)| row.as_str())
            .collect();
        Ok(inner
            .rows
            .iter()
            .filter(|(scope, r)| {
                self.visible((!scope.is_empty()).then_some(scope.as_str()))
                    && cited.contains(&r.id.as_str())
            })
            .map(|(_, r)| r.clone())
            .collect())
    }

    async fn score_lesson(&self, lesson_id: &str, helped: bool) -> Result<()> {
        // Only what this handle can see: its bucket, or global. The ids reach
        // here from model output (corroboration), and a tenant must not be
        // able to move another tenant's score by naming its id.
        let visible = |l: &&mut Lesson| {
            l.scope_key.is_none() || l.scope_key.as_deref() == self.scope.as_deref()
        };
        let mut inner = self.guard();
        if let Some(lesson) = inner
            .lessons
            .iter_mut()
            .filter(visible)
            .find(|l| l.id == lesson_id)
        {
            lesson.applied += 1;
            lesson.helped += u32::from(helped);
        }
        Ok(())
    }

    async fn score_workflow(&self, workflow_id: &str, helped: bool) -> Result<()> {
        let key = (self.bucket(), workflow_id.to_string());
        let mut inner = self.guard();
        let score = inner.scores.entry(key).or_default();
        score.applied += 1;
        score.helped += u32::from(helped);
        Ok(())
    }

    async fn workflow_score(&self, workflow_id: &str) -> Result<Score> {
        Ok(self
            .guard()
            .scores
            .get(&(self.bucket(), workflow_id.to_string()))
            .copied()
            .unwrap_or_default())
    }

    async fn link_variant(&self, parent: &str, variant: &str) -> Result<()> {
        self.guard()
            .variants
            .entry((self.bucket(), variant.to_string()))
            .or_insert_with(|| parent.to_string());
        Ok(())
    }

    async fn parent_of(&self, id: &str) -> Result<Option<String>> {
        Ok(self
            .guard()
            .variants
            .get(&(self.bucket(), id.to_string()))
            .cloned())
    }

    async fn children_of(&self, id: &str) -> Result<Vec<String>> {
        let bucket = self.bucket();
        let inner = self.guard();
        let mut found: Vec<String> = inner
            .variants
            .iter()
            .filter(|((scope, _), parent)| scope == &bucket && parent.as_str() == id)
            .map(|((_, variant), _)| variant.clone())
            .collect();
        // A HashMap has no order and `lineage` must read the same twice.
        found.sort();
        Ok(found)
    }

    async fn save_episode(&self, episode: &Episode) -> Result<()> {
        let stored = Episode {
            scope_key: self.scope.clone(),
            ..episode.clone()
        };
        let mut inner = self.guard();
        match inner
            .episodes
            .iter_mut()
            .find(|e| e.id == episode.id && e.scope_key.as_deref() == self.scope.as_deref())
        {
            Some(existing) => {
                // `started_at` and the scope are facts about creation, not
                // progress, so an update leaves them alone — matching mongo's
                // `$setOnInsert`.
                let started = existing.started_at.clone();
                let scope = existing.scope_key.clone();
                *existing = Episode {
                    started_at: started,
                    scope_key: scope,
                    ..stored
                };
            }
            None => inner.episodes.push(stored),
        }
        Ok(())
    }

    async fn episode(&self, id: &str) -> Result<Option<Episode>> {
        Ok(self
            .guard()
            .episodes
            .iter()
            .find(|e| e.id == id && e.scope_key.as_deref() == self.scope.as_deref())
            .cloned())
    }

    async fn episodes(&self, running_only: bool, page: super::Page) -> Result<Vec<Episode>> {
        let mut found: Vec<Episode> = self
            .guard()
            .episodes
            .iter()
            .filter(|e| e.scope_key.as_deref() == self.scope.as_deref())
            .filter(|e| !running_only || e.status == super::EpisodeStatus::Running)
            .cloned()
            .collect();
        // Newest first, ids breaking ties — `Page` documents that order, the
        // durable backends sort in the query, and this backend is the
        // reference the conformance suite pins. Insertion order is the
        // opposite end of the list.
        found.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(a.id.cmp(&b.id)));
        Ok(page.apply(found))
    }

    async fn save_steps(&self, row_id: &str, steps: &[crate::execute::StepRecord]) -> Result<()> {
        self.guard()
            .steps
            .insert((self.bucket(), row_id.to_string()), steps.to_vec());
        Ok(())
    }

    async fn steps(&self, row_id: &str) -> Result<Vec<crate::execute::StepRecord>> {
        Ok(self
            .guard()
            .steps
            .get(&(self.bucket(), row_id.to_string()))
            .cloned()
            .unwrap_or_default())
    }
}

/// Kept so the unused-import lint stays honest if the error type is ever needed
/// here: nothing in memory can fail, which is itself worth stating.
const _: Option<LedgerError> = None;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::conformance;

    #[tokio::test]
    async fn passes_the_conformance_suite() {
        // The same cases both durable backends pass. That the trait is
        // implementable in std alone is the point: a host writing a third
        // backend has a complete example checked by the cases theirs will be.
        conformance::run_all(&MemoryLedger::new()).await;
    }

    #[tokio::test]
    async fn passes_the_tenant_isolation_suite() {
        let store = MemoryLedger::new();
        let a = store.for_tenant("user-a");
        let b = store.for_tenant("user-b");
        conformance::run_tenants(&store, &a, &b).await;
    }

    #[tokio::test]
    async fn a_scoped_handle_shares_the_store_rather_than_copying_it() {
        // Two handles for the SAME tenant must see each other's writes — that
        // is what "shares" means. Probing it across scopes would now fail by
        // design, because rows carry the bucket that wrote them.
        let store = MemoryLedger::new();
        let one = store.for_tenant("user-a");
        let two = store.for_tenant("user-a");
        one.append(&conformance::row("ep-shared", 1, "authored"))
            .await
            .expect("append");
        assert_eq!(two.rows("ep-shared").await.expect("rows").len(), 1);
        assert!(
            store.rows("ep-shared").await.expect("rows").is_empty(),
            "and the global bucket is its own, not a union"
        );
    }

    #[tokio::test]
    async fn it_forgets_which_is_the_whole_point_of_the_name() {
        // Not a limitation being tested around — the behaviour, pinned, so the
        // difference from a durable backend is visible in the test names.
        let first = MemoryLedger::new();
        first
            .append(&conformance::row("ep-gone", 1, "authored"))
            .await
            .expect("append");
        assert_eq!(first.rows("ep-gone").await.expect("rows").len(), 1);

        let second = MemoryLedger::new();
        assert!(
            second.rows("ep-gone").await.expect("rows").is_empty(),
            "a new ledger is a new memory; nothing crosses between them"
        );
    }
}
