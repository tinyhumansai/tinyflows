//! Everything that spans runs.
//!
//! The engine's own [`tinyflows::store`] holds workflows, run records, notes and
//! proposals — all of it *about one run* or one document. This holds the other
//! half: what was tried across attempts, what generalised out of that, and
//! which stored procedures have actually earned their place.
//!
//! Kept as a separate trait rather than as more methods on `WorkflowStore`, for
//! two reasons that are really one. The engine's store is upstream's type and a
//! merge should never contend with our additions; and the boundary this project
//! rests on — *the engine may know about one run, anything that spans runs is
//! ours* — is worth having in the type system rather than in a document.
//!
//! Three implementations ship. `sqlite` and `mongo` are behind features,
//! because the choice is the host's and a deployment that wants one should not
//! build the other's driver. [`memory`] is always compiled, needs no driver,
//! and **forgets everything on restart** — it exists so the crate is usable the
//! moment it is added, and it is never selected for you.
//!
//! All three are checked by the same conformance suite ([`conformance`]), so
//! "it works on sqlite" cannot quietly mean "it works only on sqlite" — and so
//! a host writing a fourth backend has a std-only reference implementation
//! checked by the cases theirs will be.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub mod memory;
#[cfg(feature = "mongo")]
pub mod mongo;
#[cfg(feature = "sqlite")]
pub mod sqlite;

pub mod conformance;

/// What went wrong reaching the ledger.
///
/// Deliberately coarse. A caller can retry or give up; it cannot repair a
/// backend, so a taxonomy of driver errors would be detail nobody branches on.
#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    /// The backend refused or was unreachable.
    #[error("ledger backend: {0}")]
    Backend(String),
    /// Something was stored that no longer parses — a schema moved under us.
    #[error("ledger holds a row it cannot read: {0}")]
    Corrupt(String),
}

/// Convenience alias for ledger results.
pub type Result<T> = std::result::Result<T, LedgerError>;

/// One attempt, recorded as it finishes.
///
/// The unit is an *attempt*, not a run: a single episode may run three
/// workflows and author a fourth, and the exclusion list that stops attempt
/// four repeating attempt two is built from these rows.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LedgerRow {
    /// Assigned by the backend on append; empty when not yet stored.
    #[serde(default)]
    pub id: String,
    /// The episode this attempt belongs to — one goal, many attempts.
    pub episode: String,
    /// 1-based, so a row reads the way a person counts.
    pub attempt: u32,
    /// [`crate::contracts::Approach::signature`]. What the exclusion list is
    /// built from, and what a lesson is keyed against.
    pub approach_sig: String,
    /// The approach in a sentence, for a human reading the trail.
    #[serde(default)]
    pub approach_desc: String,
    /// The workflow that ran, when one did. Absent for an authoring attempt
    /// that never reached a graph.
    #[serde(default)]
    pub workflow_id: Option<String>,
    /// What happened, in the judge's words.
    #[serde(default)]
    pub outcome: String,
    /// Why it fell short. Empty when it did not.
    #[serde(default)]
    pub cause: String,
    /// What it cost, in whatever unit the host counts. Zero is "not measured",
    /// which is honest; a made-up estimate is not.
    #[serde(default)]
    pub cost_usd: f64,
    /// RFC 3339. Supplied by the caller so a frozen clock can drive tests.
    pub at: String,
    /// Whether the judge called this attempt satisfied.
    ///
    /// A field rather than `outcome == "satisfied"`: that string match works
    /// and is one reworded line away from silently reporting every episode as
    /// failed.
    #[serde(default)]
    pub satisfied: bool,
    /// Whether it got closer than the state before it.
    ///
    /// Stored because the stall rule is computed from it, and an episode a
    /// restarted process cannot recompute is an episode it has to start over.
    #[serde(default)]
    pub advanced: bool,
}

/// The four kinds of thing an episode can teach.
///
/// A closed set because retrieval filters on it and a prompt asks for it; an
/// open one becomes a synonym pile within a week.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LessonKind {
    /// X works where Y fails. Lands in the next plan's approach.
    Strategy,
    /// A limit no approach here can cross. Rules approaches out.
    Constraint,
    /// A way this silently looks done when it is not. Becomes something the
    /// next run checks for.
    FailureMode,
    /// An estimate that was systematically wrong, and by how much.
    Calibration,
}

impl LessonKind {
    /// Reads a model's answer, defaulting to the least actionable kind.
    ///
    /// Unrecognised becomes `Strategy` rather than an error: a lesson with a
    /// misfiled kind is still worth keeping, and refusing the write loses it.
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "constraint" => Self::Constraint,
            "failure_mode" => Self::FailureMode,
            "calibration" => Self::Calibration,
            _ => Self::Strategy,
        }
    }
}

/// Something a *different* task could act on.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Lesson {
    /// Assigned by the backend on promote.
    #[serde(default)]
    pub id: String,
    /// Which kind, so retrieval can filter.
    pub kind: LessonKind,
    /// What decides whether this is ever found again — and the easiest thing
    /// to get wrong in both directions. It must describe the *class* of
    /// situation: "a CPU-bound scan over ~1M items with a sub-100ms target",
    /// never "Project Euler 14" (matches once, never again) and never "a task
    /// that needs to be fast" (matches everything, says nothing).
    pub trigger: String,
    /// Why it is true.
    #[serde(default)]
    pub mechanism: String,
    /// What to do about it.
    pub claim: String,
    /// How many times it was put in front of a planner.
    #[serde(default)]
    pub applied: u32,
    /// How many of those ended satisfied.
    #[serde(default)]
    pub helped: u32,
    /// Whose lesson this is. `None` is global — visible to everyone.
    ///
    /// Never set by a caller: [`Ledger::promote`] stamps it from the handle's
    /// own [`scope`](Ledger::scope). A lesson's `trigger` and `claim` are free
    /// text drawn from one tenant's episode and can name their repositories,
    /// paths and internals, so which tenant owns it is not a decision a model
    /// or a caller gets to make.
    #[serde(default)]
    pub scope_key: Option<String>,
}

impl Lesson {
    /// Both numbers are kept rather than a rate, because 1/1 and 40/40 are the
    /// same rate and are not the same evidence. This is for ordering only.
    #[must_use]
    pub fn help_rate(&self) -> f64 {
        if self.applied == 0 {
            0.0
        } else {
            f64::from(self.helped) / f64::from(self.applied)
        }
    }
}

/// How a stored workflow has actually performed.
///
/// Not on `WorkflowRecord`: a score is a fact that spans runs, and the engine's
/// record is a fact about one document. Keyed by workflow id on our side.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Score {
    /// Times this workflow was chosen and run.
    pub applied: u32,
    /// Times that ended satisfied.
    pub helped: u32,
}

impl Score {
    /// Both numbers are kept rather than a rate, because 1/1 and 40/40 are the
    /// same rate and are not the same evidence. This is for ordering only.
    #[must_use]
    pub fn help_rate(&self) -> f64 {
        if self.applied == 0 {
            0.0
        } else {
            f64::from(self.helped) / f64::from(self.applied)
        }
    }
}

/// How far up a variant chain [`Ledger::lineage`] will walk before giving up.
pub const MAX_LINEAGE_DEPTH: usize = 8;

/// How many members of one family [`Ledger::lineage`] will return.
pub const MAX_FAMILY: usize = 64;

/// The exclusion list, from rows already in hand.
///
/// [`Ledger::tried`] is this over a fresh read, which is the right shape for a
/// caller that wants only the signatures. A caller that also renders the
/// history — [`crate::intake::decide`] does both — reads the rows once and
/// calls this, rather than paying for the same query twice per attempt.
///
/// First-seen order, deduplicated. Order matters because it is rendered into a
/// prompt, and a list that reshuffles between attempts is one a planner cannot
/// be reasoned about against.
#[must_use]
pub fn signatures(rows: &[LedgerRow]) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for row in rows {
        if !seen.contains(&row.approach_sig) {
            seen.push(row.approach_sig.clone());
        }
    }
    seen
}

/// A window onto a list that grows without bound.
///
/// Only [`Ledger::episodes`] takes one. An episode's *rows* are bounded by
/// [`crate::contracts::Budget::attempts`] — a dozen — so paging them would be
/// ceremony around a list that cannot get long. A tenant's episodes accumulate
/// forever, which is a different thing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Page {
    /// How many at most.
    pub limit: usize,
    /// How many to skip, newest first.
    pub offset: usize,
}

impl Page {
    /// Everything. What the loop's own recovery pass wants.
    pub const ALL: Self = Self {
        limit: usize::MAX,
        offset: 0,
    };

    /// The first `n`, from the top.
    #[must_use]
    pub fn first(n: usize) -> Self {
        Self {
            limit: n,
            offset: 0,
        }
    }

    /// Apply to an already-ordered list.
    ///
    /// Applied in the backend after ordering rather than pushed into each
    /// query, because two of the three have no query language and the third
    /// would then be the only one whose paging could disagree.
    #[must_use]
    pub fn apply<T>(self, mut items: Vec<T>) -> Vec<T> {
        if self.offset >= items.len() {
            return Vec::new();
        }
        items.drain(..self.offset);
        items.truncate(self.limit);
        items
    }
}

/// How an episode ended, or that it has not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state", content = "reason")]
pub enum EpisodeStatus {
    /// Still going.
    Running,
    /// The goal was met.
    Satisfied,
    /// Stopped without success, for the reason
    /// [`crate::closing::Next::StandDown`] gave.
    StoodDown(String),
}

/// One goal, from the first attempt to whatever ended it.
///
/// The loop's own checkpoint, and the thing that makes an episode survive the
/// process running it. Everything here is either unrecoverable from the rows
/// (the goal) or expensive and error-prone to recompute (the counters), which
/// is the test for what belongs on it.
///
/// Not the engine's `Checkpointer`: that holds mid-run superstep state for
/// `engine::resume`, which this crate deliberately does not use. This is
/// between runs, which is the boundary the whole crate sits on.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Episode {
    /// The caller's id. Minted by whoever owns episodes — a service, a CLI —
    /// and opaque here.
    pub id: String,
    /// What was asked. Unrecoverable from the rows, so a restart without it
    /// cannot continue.
    pub goal: crate::contracts::Goal,
    /// Whose it is, stamped from the handle's scope on write.
    #[serde(default)]
    pub scope_key: Option<String>,
    /// Where it is.
    pub status: EpisodeStatus,
    /// Attempts spent.
    #[serde(default)]
    pub attempt: u32,
    /// Consecutive attempts that made no progress.
    #[serde(default)]
    pub stalled: u32,
    /// RFC 3339, caller-supplied.
    pub started_at: String,
    /// RFC 3339, caller-supplied.
    pub updated_at: String,
}

/// Everything that spans runs.
///
/// Every method is fallible and none of them panics on an absent row: a missing
/// lesson or an unknown workflow is an empty answer, not an error. A loop that
/// cannot read its own history should degrade to a first-time run, never stop.
#[async_trait]
pub trait Ledger: Send + Sync {
    /// Whose knowledge this handle reads and writes. `None` is the global
    /// bucket.
    ///
    /// The scope lives on the handle rather than on every method because the
    /// failure it prevents is *forgetting to pass it*. One `for_tenant` at the
    /// edge of a request is a thing a reviewer can see; six scope arguments
    /// threaded through intake and closing is a thing that goes wrong once and
    /// leaks one tenant's lessons into another's prompt.
    ///
    /// One rule, everywhere:
    ///
    /// * **writes** go to this handle's bucket;
    /// * **reads** return this handle's bucket plus the global one.
    ///
    /// An unscoped handle's bucket *is* global, so a single-tenant deployment
    /// that never calls `for_tenant` reads back exactly what it wrote and
    /// nothing changes for it.
    ///
    /// Episode rows are not affected: they are already keyed by episode, and
    /// [`tried`](Ledger::tried) reads one episode at a time.
    fn scope(&self) -> Option<&str> {
        None
    }

    /// Record one finished attempt. Returns the assigned id.
    async fn append(&self, row: &LedgerRow) -> Result<String>;

    /// Every attempt in one episode, oldest first.
    async fn rows(&self, episode: &str) -> Result<Vec<LedgerRow>>;

    /// The approach signatures already spent on this episode.
    ///
    /// This is the exclusion list, and it is the reason the ledger exists at
    /// all: without it a planner re-proposes attempt two's idea at attempt four
    /// in slightly different words, and the run pays twice for the same dead
    /// end.
    async fn tried(&self, episode: &str) -> Result<Vec<String>> {
        Ok(signatures(&self.rows(episode).await?))
    }

    /// Keep a lesson, citing the rows it was drawn from.
    ///
    /// The stored lesson's [`scope_key`](Lesson::scope_key) is this handle's
    /// [`scope`](Ledger::scope), whatever the argument says.
    ///
    /// A claim with no rows behind it is a guess, so the citation is part of
    /// the call rather than an optional extra.
    async fn promote(&self, lesson: &Lesson, cites: &[String]) -> Result<String>;

    /// Lessons in scope, optionally of one kind.
    async fn lessons(&self, kind: Option<LessonKind>) -> Result<Vec<Lesson>>;

    /// The rows a lesson cited, for a reader arguing with it.
    async fn evidence(&self, lesson_id: &str) -> Result<Vec<LedgerRow>>;

    /// Note that a lesson was shown to a planner, and whether that run ended
    /// satisfied. Both counters move; only the second is conditional.
    async fn score_lesson(&self, lesson_id: &str, helped: bool) -> Result<()>;

    /// The same for a workflow. This is the missing rung: without it nothing
    /// distinguishes a procedure that has worked forty times from one that has
    /// never run, and a promotion gate has no evidence to read.
    async fn score_workflow(&self, workflow_id: &str, helped: bool) -> Result<()>;

    /// How a workflow has performed. Unknown ids answer `Score::default()`
    /// rather than erroring — a workflow nobody has run yet is 0/0, not a bug.
    async fn workflow_score(&self, workflow_id: &str) -> Result<Score>;

    /// Record that `variant` was derived from `parent`.
    ///
    /// Lineage lives here rather than on `WorkflowRecord` for the usual reason:
    /// the engine's record is a fact about one document, and *this graph came
    /// from that one after it fell short* is a fact that spans runs. It is also
    /// what stops a repaired family from filling the catalogue with six
    /// near-identical rows a planner has to choose between blindly.
    ///
    /// Idempotent: re-linking the same pair is a no-op, because a repair that
    /// converges on an existing variant id will try.
    async fn link_variant(&self, parent: &str, variant: &str) -> Result<()>;

    /// What `id` was derived from, if anything.
    async fn parent_of(&self, id: &str) -> Result<Option<String>>;

    /// What was derived directly from `id`.
    async fn children_of(&self, id: &str) -> Result<Vec<String>>;

    /// Write an episode, creating or replacing it.
    ///
    /// The [`scope_key`](Episode::scope_key) stored is this handle's, whatever
    /// the argument says — the same rule as [`promote`](Ledger::promote), for
    /// the same reason.
    async fn save_episode(&self, episode: &Episode) -> Result<()>;

    /// Read one episode, if this handle's scope can see it.
    ///
    /// This is resume: a process that restarts mid-episode reads the goal and
    /// the counters back and carries on, rather than starting the goal over
    /// with a ledger that says it has already been attempted four times.
    async fn episode(&self, id: &str) -> Result<Option<Episode>>;

    /// Every episode in this handle's scope, optionally filtered by state.
    ///
    /// `Running` on boot is the recovery list. Without it a deploy silently
    /// abandons every episode that was in flight — the rows stay, nothing ever
    /// looks at them again, and the goal is never answered.
    async fn episodes(&self, running_only: bool, page: Page) -> Result<Vec<Episode>>;

    /// Keep an attempt's per-node record, addressed by its ledger row.
    ///
    /// The transcript: what each node emitted, whether it errored, how long it
    /// took, and which of its bindings resolved to null. The judge is shown a
    /// bounded projection of it and the ledger row holds a sentence; this is
    /// the detail behind both, and without it "show me what that attempt did"
    /// has nothing to answer with.
    ///
    /// **One record per step, never one blob per attempt.** A `loop` node
    /// produces a step per iteration, and at
    /// [`RECORD_BUDGET`](crate::execute::RECORD_BUDGET) that reaches megabytes
    /// — past what a Mongo document may hold. A blob would work on sqlite, work
    /// in testing, and fail in production on exactly the runs most worth
    /// reading.
    async fn save_steps(&self, row_id: &str, steps: &[crate::execute::StepRecord]) -> Result<()>;

    /// One attempt's per-node record, in execution order.
    async fn steps(&self, row_id: &str) -> Result<Vec<crate::execute::StepRecord>>;

    /// Every workflow in `id`'s family, **root first**, including `id`.
    ///
    /// Works from any member: it walks up to the root, then breadth-first down.
    /// Both walks are bounded, so a cycle written by a buggy caller costs a
    /// truncated answer rather than a loop that never returns — the ledger is
    /// read on the hot path of every attempt, and a hang there stops
    /// everything.
    async fn lineage(&self, id: &str) -> Result<Vec<String>> {
        let mut root = id.to_string();
        for _ in 0..MAX_LINEAGE_DEPTH {
            match self.parent_of(&root).await? {
                Some(parent) if parent != root => root = parent,
                _ => break,
            }
        }
        let mut family = vec![root];
        let mut next = 0;
        while next < family.len() && family.len() < MAX_FAMILY {
            for child in self.children_of(&family[next]).await? {
                if !family.contains(&child) {
                    family.push(child);
                }
            }
            next += 1;
        }
        Ok(family)
    }
}

#[cfg(test)]
mod signature_tests {
    use super::{LedgerRow, signatures};

    fn row(attempt: u32, sig: &str) -> LedgerRow {
        LedgerRow {
            id: format!("r{attempt}"),
            episode: "ep".into(),
            attempt,
            approach_sig: sig.into(),
            approach_desc: String::new(),
            workflow_id: None,
            outcome: String::new(),
            cause: String::new(),
            cost_usd: 0.0,
            at: "2026-01-01T00:00:00Z".into(),
            satisfied: false,
            advanced: false,
        }
    }

    #[test]
    fn an_approach_tried_twice_appears_once() {
        let got = signatures(&[
            row(1, "selected:weekly"),
            row(2, "authored:aaa"),
            row(3, "selected:weekly"),
        ]);
        assert_eq!(got, vec!["selected:weekly", "authored:aaa"]);
    }

    #[test]
    fn first_seen_order_is_kept() {
        // It is rendered into a prompt, and a list that reshuffles between
        // attempts is one a planner cannot be reasoned about against.
        let got = signatures(&[row(1, "c"), row(2, "a"), row(3, "b")]);
        assert_eq!(got, vec!["c", "a", "b"]);
    }

    #[test]
    fn no_rows_is_an_empty_list_rather_than_a_surprise() {
        assert!(signatures(&[]).is_empty());
    }

    #[tokio::test]
    async fn the_trait_method_agrees_with_the_function_it_now_calls() {
        // `tried` is this over a fresh read. If the two ever disagree, one
        // caller's exclusion list is not the other's.
        use super::Ledger;
        let ledger = super::memory::MemoryLedger::new();
        for (attempt, sig) in [
            (1u32, "selected:weekly"),
            (2, "authored:aaa"),
            (3, "selected:weekly"),
        ] {
            ledger.append(&row(attempt, sig)).await.expect("append");
        }
        let rows = ledger.rows("ep").await.expect("rows");
        assert_eq!(ledger.tried("ep").await.expect("tried"), signatures(&rows));
    }
}
