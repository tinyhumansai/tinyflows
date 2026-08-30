//! The measured quantity: attempts-to-success **declining across a family**.
//!
//! Not the success rate. On disjoint tasks a learning loop and a plain retry
//! loop produce identical numbers — both solve ten unrelated problems once
//! each — so an eval without within-distribution repetition proves nothing
//! about learning, whatever its success rate says.
//!
//! So the eval is two arms over one family of related tasks, in the same
//! order, differing only in whether anything survives between episodes, and
//! the number that decides it is the **slope**.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::driver::Finished;
use crate::ledger::{EpisodeStatus, LedgerRow};

/// How an episode actually ended, once an outside check has its say.
///
/// Three states, not two, and the difference is not academic: the first
/// version of this collapsed the last two and reported a run that produced the
/// wrong answer as a success because the judge had accepted it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The judge was satisfied and nothing outside contradicted it.
    Solved,
    /// Right, but the judge would not accept it. A judging defect, not a
    /// solving one — and counting it as a failure hides that.
    Refused,
    /// The judge was satisfied and the answer is wrong. The most expensive
    /// state to be blind to, because everything downstream believes it.
    Wrong,
    /// It did not solve the task and nothing says otherwise.
    Failed,
}

/// One episode of one arm, as the measurement sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct Episode {
    /// Which task, so a family can be read back task by task.
    pub task: String,
    /// The family it belongs to. Two arms are only comparable within one.
    pub family: String,
    /// Attempts the loop spent.
    pub attempts: u32,
    /// What the judge concluded.
    pub satisfied: bool,
    /// An outside check on the work, when the eval has one — the published
    /// answer, a test suite, a diff that must exist.
    ///
    /// `None` means the eval cannot check, which is honest and common. It is
    /// *not* the same as `Some(true)`: an unchecked run is taken at the
    /// judge's word, and the report says which runs those were.
    pub verified: Option<bool>,
    /// What the arm spent on this episode, in the host's unit. Zero means not
    /// measured.
    pub cost_usd: f64,
    /// Whether a stored workflow served it, rather than one being authored.
    pub used_workflow: bool,
    /// How many lessons were put in front of a planner during it.
    pub lessons_applied: u32,
}

impl Episode {
    /// Read an episode off what the loop already records.
    ///
    /// `rows` are that episode's ledger rows. Everything here comes from them
    /// or from [`Finished`] rather than from the runner, because the ledger is
    /// what the loop is claimed to learn from — measuring anything else would
    /// measure a parallel bookkeeping nobody uses.
    #[must_use]
    pub fn of(task: &str, family: &str, finished: &Finished, rows: &[LedgerRow]) -> Self {
        Self {
            task: task.to_string(),
            family: family.to_string(),
            attempts: finished.attempts,
            satisfied: matches!(finished.status, EpisodeStatus::Satisfied),
            verified: None,
            cost_usd: rows.iter().map(|row| row.cost_usd).sum(),
            // A stored workflow served this episode if any attempt named one.
            // The last attempt is the one that succeeded, but an earlier
            // workflow attempt is still the store having been used.
            used_workflow: rows.iter().any(|row| row.workflow_id.is_some()),
            lessons_applied: 0,
        }
    }

    /// Record an outside check on the work.
    #[must_use]
    pub fn checked(mut self, verified: bool) -> Self {
        self.verified = Some(verified);
        self
    }

    /// How many lessons the planners were shown across this episode.
    #[must_use]
    pub fn with_lessons_applied(mut self, applied: u32) -> Self {
        self.lessons_applied = applied;
        self
    }

    /// Whether this counts as a success for the measurement.
    ///
    /// The judge, unless an outside check says otherwise. A run the judge
    /// accepted and the world refuted is not a data point about converging
    /// faster; it is a data point about the judge.
    #[must_use]
    pub fn solved(&self) -> bool {
        self.satisfied && self.verified != Some(false)
    }

    /// The four-way reading of how it ended.
    #[must_use]
    pub fn outcome(&self) -> Outcome {
        match (self.satisfied, self.verified) {
            (true, Some(false)) => Outcome::Wrong,
            (true, _) => Outcome::Solved,
            (false, Some(true)) => Outcome::Refused,
            (false, _) => Outcome::Failed,
        }
    }
}

/// One arm of the experiment — learning on, or learning off.
#[derive(Debug, Clone, Default)]
pub struct Series {
    /// What to call this arm in the report.
    pub label: String,
    /// Its episodes, in the order they ran. Order is the independent variable,
    /// so this is a sequence and never a set.
    pub episodes: Vec<Episode>,
}

impl Series {
    /// An empty arm under `label`.
    #[must_use]
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            episodes: Vec::new(),
        }
    }

    /// How many episodes counted as solved.
    #[must_use]
    pub fn solved(&self) -> usize {
        self.episodes.iter().filter(|e| e.solved()).count()
    }

    /// Mean attempts over the episodes that solved, or `None` if none did.
    ///
    /// Failures are excluded because an attempt count on a task that never
    /// succeeded is a budget, not a measurement of how quickly it converged.
    ///
    /// `None` rather than `0.0`, and the distinction is load-bearing: zero is
    /// the *best possible* attempt count, so an arm that solved nothing would
    /// beat every arm that solved something. Absence has to be absence.
    #[must_use]
    pub fn mean_attempts(&self) -> Option<f64> {
        let wins: Vec<f64> = self
            .episodes
            .iter()
            .filter(|e| e.solved())
            .map(|e| f64::from(e.attempts))
            .collect();
        if wins.is_empty() {
            return None;
        }
        Some(wins.iter().sum::<f64>() / wins.len() as f64)
    }

    /// Total spend divided by episodes solved.
    ///
    /// The numerator is *every* episode's cost, including the ones that failed:
    /// spending on failures is real spending, and an arm that burns three
    /// episodes to win one has not won cheaply.
    #[must_use]
    pub fn cost_per_solve(&self) -> f64 {
        let solved = self.solved();
        if solved == 0 {
            return 0.0;
        }
        self.episodes.iter().map(|e| e.cost_usd).sum::<f64>() / solved as f64
    }

    /// The fraction of episodes a stored workflow served.
    #[must_use]
    pub fn workflow_hit_rate(&self) -> f64 {
        if self.episodes.is_empty() {
            return 0.0;
        }
        let hits = self.episodes.iter().filter(|e| e.used_workflow).count();
        hits as f64 / self.episodes.len() as f64
    }

    /// How many episodes ended each way.
    #[must_use]
    pub fn outcomes(&self) -> BTreeMap<&'static str, usize> {
        let mut counts: BTreeMap<&'static str, usize> =
            [("solved", 0), ("refused", 0), ("wrong", 0), ("failed", 0)]
                .into_iter()
                .collect();
        for episode in &self.episodes {
            let key = match episode.outcome() {
                Outcome::Solved => "solved",
                Outcome::Refused => "refused",
                Outcome::Wrong => "wrong",
                Outcome::Failed => "failed",
            };
            *counts.entry(key).or_insert(0) += 1;
        }
        counts
    }

    /// Least-squares slope of attempts against position in the sequence, or
    /// `None` when fewer than two episodes solved.
    ///
    /// Negative means converging faster as the family progresses, which is the
    /// entire claim. A flat line is a retry loop wearing a learning costume.
    ///
    /// Fitted over the solved episodes only, at their *original* positions —
    /// a failure in the middle is a gap in the sequence, not a shift of
    /// everything after it.
    ///
    /// `None` rather than `0.0` for the same reason `mean_attempts` is
    /// optional. Two points make a line and one makes an anecdote, and
    /// reporting the anecdote as "flat" reads as *evidence of no learning*
    /// when it is the absence of evidence either way — which is exactly how
    /// an arm that solved nothing came to tie with an arm that solved four.
    #[must_use]
    pub fn trend(&self) -> Option<f64> {
        let wins: Vec<(f64, f64)> = self
            .episodes
            .iter()
            .enumerate()
            .filter(|(_, e)| e.solved())
            .map(|(i, e)| (i as f64, f64::from(e.attempts)))
            .collect();
        if wins.len() < 2 {
            return None;
        }
        let mean_x = wins.iter().map(|(x, _)| x).sum::<f64>() / wins.len() as f64;
        let mean_y = wins.iter().map(|(_, y)| y).sum::<f64>() / wins.len() as f64;
        let denom: f64 = wins.iter().map(|(x, _)| (x - mean_x).powi(2)).sum();
        if denom == 0.0 {
            return None;
        }
        Some(
            wins.iter()
                .map(|(x, y)| (x - mean_x) * (y - mean_y))
                .sum::<f64>()
                / denom,
        )
    }
}

/// Two arms over the same family, differing only in whether learning is on.
#[derive(Debug, Clone, Default)]
pub struct Experiment {
    /// The family both arms ran.
    pub family: String,
    /// The arms, by label. Insertion-ordered for a stable report.
    pub arms: BTreeMap<String, Series>,
}

/// What an experiment concluded, and on which evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The treatment arm solved episodes the control could not, without
    /// costing more attempts on the ones both solved.
    SolvedMore,
    /// Both arms converged and the treatment arm converged faster, ending up
    /// no worse in attempts.
    Converged,
    /// No difference the evidence can support — including a tie, and including
    /// too few solved episodes to fit anything.
    NoEvidence,
}

/// The conventional label for the arm that keeps what it learns.
pub const LEARNING_ON: &str = "learning_on";
/// The conventional label for the arm that starts each episode empty.
pub const LEARNING_OFF: &str = "learning_off";

impl Experiment {
    /// An experiment over `family` with no episodes yet.
    #[must_use]
    pub fn new(family: impl Into<String>) -> Self {
        Self {
            family: family.into(),
            arms: BTreeMap::new(),
        }
    }

    /// Add an episode to an arm, creating the arm on first use.
    pub fn record(&mut self, label: &str, episode: Episode) {
        self.arms
            .entry(label.to_string())
            .or_insert_with(|| Series::new(label))
            .episodes
            .push(episode);
    }

    /// The claim, stated as a comparison rather than an absolute.
    #[must_use]
    pub fn learning_helps(&self) -> bool {
        matches!(
            self.compare(LEARNING_ON, LEARNING_OFF),
            Verdict::Converged | Verdict::SolvedMore
        )
    }

    /// Why the comparison came out the way it did.
    ///
    /// Two ways an arm can show that attempts-to-success fell, and the first
    /// version of this only knew one of them. Running the eval produced a
    /// treatment arm that solved four of five against a control that solved
    /// **none**, and it reported no evidence — because both slopes read as
    /// `0.0`, one meaning "flat" and the other meaning "two points are needed
    /// for a line and there was one". Going from *never* to *once* is the
    /// largest fall in attempts-to-success there is; a measure that calls it a
    /// tie is measuring the wrong quantity.
    ///
    /// So, in order:
    ///
    /// 1. **Solved more.** Strictly more episodes solved, and no worse per
    ///    solve where both arms have a figure. Solving what the other arm
    ///    cannot is convergence in the only sense that matters, and no slope
    ///    is needed to see it.
    /// 2. **Converged.** Both arms have a real trend, the treatment arm's is
    ///    lower, and it also ends up no worse in attempts — a curve that bends
    ///    but never converges is not a win: twenty falling to ten loses to a
    ///    flat two.
    ///
    /// Anything else is [`Verdict::NoEvidence`], including a tie, which is the
    /// answer disjoint tasks must produce.
    #[must_use]
    pub fn compare(&self, on: &str, off: &str) -> Verdict {
        let (Some(on), Some(off)) = (self.arms.get(on), self.arms.get(off)) else {
            // One arm compares with nothing, and an experiment that claims a
            // win from a single arm is the failure this module exists to stop.
            return Verdict::NoEvidence;
        };
        // "No worse per solve" only binds when both arms have solved
        // something. An arm with no wins has no attempt count, and treating
        // its absence as zero would let it out-argue every arm that worked.
        let no_worse = match (on.mean_attempts(), off.mean_attempts()) {
            (Some(a), Some(b)) => a <= b,
            _ => true,
        };
        if on.solved() > off.solved() && no_worse {
            return Verdict::SolvedMore;
        }
        match (on.trend(), off.trend()) {
            (Some(a), Some(b)) if a < b && no_worse => Verdict::Converged,
            _ => Verdict::NoEvidence,
        }
    }

    /// Every claim the experiment can speak to, as JSON.
    #[must_use]
    pub fn report(&self) -> Value {
        let arms: serde_json::Map<String, Value> = self
            .arms
            .iter()
            .map(|(label, series)| {
                (
                    label.clone(),
                    json!({
                        "solved": series.solved(),
                        "episodes": series.episodes.len(),
                        "mean_attempts": series.mean_attempts().map(|v| round(v, 3)),
                        "slope": series.trend().map(|v| round(v, 4)),
                        "cost_per_solve": round(series.cost_per_solve(), 4),
                        "workflow_hit_rate": round(series.workflow_hit_rate(), 3),
                        "outcomes": series.outcomes(),
                    }),
                )
            })
            .collect();
        json!({
            "family": self.family,
            "learning_helps": self.learning_helps(),
            "verdict": match self.compare(LEARNING_ON, LEARNING_OFF) {
                Verdict::SolvedMore => "solved_more",
                Verdict::Converged => "converged",
                Verdict::NoEvidence => "no_evidence",
            },
            "arms": arms,
        })
    }
}

/// Round for a report a person reads. Never used in a comparison.
fn round(value: f64, places: u32) -> f64 {
    let factor = 10_f64.powi(places as i32);
    (value * factor).round() / factor
}
