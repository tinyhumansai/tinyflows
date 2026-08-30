//! Running one attempt, and coming back with something the judge can read.
//!
//! The middle of the loop, and deliberately the thinnest part of it. Intake
//! decided *what* to run; closing decides what it *meant*. This only runs it —
//! it holds no opinion about the result, reads no history, and makes no
//! decision the other two layers could make instead.
//!
//! Its one real job is that the engine hands back a [`RunOutcome`] and the
//! judge needs an [`Evidence`], and the difference between those is where runs
//! get misjudged.
//!
//! **A run is observed, always.** [`RunOutcome`] alone says the graph finished;
//! it does not say a binding resolved to null, that an `on_error` policy
//! swallowed a failure, or that half the nodes never executed. Those come from
//! [`diagnose`](tinyflows::diagnostics::diagnose), which needs the run's steps, which only exist if an observer
//! was attached. A run without one produces a green outcome and a blank
//! diagnosis — and a blank diagnosis is not "nothing was wrong", it is "nobody
//! looked". Every gate downstream reads it: the judge's findings, the three
//! mechanical verdicts, and [`crate::closing::graph_is_suspect`], which decides
//! whether a repair is even proposed.
//!
//! **An engine error is an attempt, not an escape.** [`run_attempt`] does not
//! return a `Result`. A graph that failed to compile or blew up mid-run still
//! has to reach `close()` and leave a ledger row, or the exclusion list never
//! learns it was tried and the next pass proposes it again in slightly
//! different words. The error becomes evidence like everything else.
//!
//! # Why no checkpointer
//!
//! The plan named [`tinyflows::engine::run_with_checkpointer`] for this phase.
//! It is the wrong entry point today, for two reasons that compound.
//!
//! It installs a `NoopObserver` — so taking it costs the diagnosis, and with it
//! every gate listed above. The variant that keeps both is
//! `run_with_checkpointer_journaled_observed`, which also demands a journal.
//!
//! And what a checkpointer buys is durable *resume*, which this crate does not
//! use. Note *use*, not *lack*: `resume_with_checkpointer` genuinely continues
//! from an interrupt boundary rather than replaying — it is `engine::resume`,
//! the HITL convenience, that re-runs every node before the gate. Our retry is
//! a new run of a new graph by choice, because a retry is a different idea and
//! not a continuation of the last one. So the cost is immediate and the benefit
//! is for a path we do not take.
//!
//! When HITL parking is wired upstream this becomes a one-line swap to the
//! journaled variant. Until then, taking a durability guarantee we cannot use
//! in exchange for the diagnosis we depend on is a bad trade made quietly.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use tinyflows::caps::Capabilities;
use tinyflows::compiler::compile;
use tinyflows::diagnostics::{Diagnosis, capturing};
use tinyflows::engine::{RunInput, RunOutcome, run_with_observer};

use crate::closing::Evidence;
use crate::intake::Attempt;

pub mod wire;
pub use wire::{PROMPT_BUDGET, RECORD_BUDGET, RunReport, RunRequest, StepOutcome, StepRecord};

/// What changed outside the run, according to the host.
///
/// The engine cannot answer this: it hands back run state, not a view of the
/// machine. A file written, a commit made, a service called — that is the
/// difference between a run that did the job and one that reported success
/// having done nothing, and it is the only evidence that comes from outside the
/// system being judged.
///
/// Two calls rather than one, because *what changed* is a comparison and needs
/// a before. A single "what is dirty now" reading cannot distinguish this run's
/// work from what was already on disk when it started.
///
/// Both methods default to empty, so a host that cannot say anything gets
/// honest silence for free — `Evidence` treats an empty `changed` as "nothing
/// reported", never as "nothing happened".
#[async_trait]
pub trait Workspace: Send + Sync {
    /// Take a baseline before the run. Opaque: a commit sha, a manifest hash,
    /// a timestamp — whatever this host can compare against later.
    async fn mark(&self) -> String {
        String::new()
    }

    /// Describe what changed since `mark`, for a reader.
    ///
    /// Prose, not a format anything parses. It is rendered into the judge's
    /// prompt and stored nowhere.
    async fn changed_since(&self, _mark: &str) -> String {
        String::new()
    }
}

/// A host with nothing to report.
///
/// The honest default, and the right one for a workflow that touches only
/// network services. Judging then rests on the run output and the diagnosis
/// alone, which is a weaker position — worth knowing you are in.
pub struct Unobserved;

impl Workspace for Unobserved {}

/// One attempt, run.
///
/// Owns the outcome and diagnosis so [`evidence`](Self::evidence) can hand out
/// a borrowed [`Evidence`] without the caller keeping three variables alive.
#[derive(Debug, Clone)]
pub struct Ran {
    /// What the run amounted to, reconstructed from the steps and bounded for
    /// reading. Not the engine's own outcome value — see
    /// [`RunReport::into_ran`].
    pub outcome: RunOutcome,
    /// The engine's reading of what the steps actually did.
    pub diagnosis: Diagnosis,
    /// What the host says changed. Empty when it does not say.
    pub changed: String,
    /// The engine error, when the run did not complete.
    ///
    /// Present *and* recorded inside `outcome.output` under `error`, so the
    /// judge sees it through the ordinary evidence rendering rather than
    /// needing a special case. A caller that wants to branch on it — a retry
    /// that distinguishes "the graph is broken" from "the work fell short" —
    /// reads it here.
    pub failed: Option<String>,
    /// Every node activation, at full record fidelity. The per-node transcript:
    /// what to archive, and richer than what the judge is shown.
    pub steps: Vec<StepRecord>,
    /// What the run cost, in the runner's unit. Zero means not measured.
    pub cost_usd: f64,
    /// Where this run stopped, when it failed at a node and the host kept a
    /// resumable boundary for it.
    ///
    /// `None` is always allowed and is the right answer for a host with no
    /// checkpointer, a run that completed, or one that broke somewhere a node
    /// boundary cannot describe. The loop treats it as "start the next attempt
    /// from the trigger", which is what every attempt did before continuing
    /// existed.
    pub resume: Option<crate::contracts::ResumePoint>,
}

impl Ran {
    /// The three sources, as the judge takes them.
    #[must_use]
    pub fn evidence(&self) -> Evidence<'_> {
        Evidence {
            outcome: &self.outcome,
            diagnosis: &self.diagnosis,
            changed: self.changed.clone(),
            failed: self.failed.clone(),
        }
    }
}

/// Whatever runs a graph.
///
/// The port the loop calls, and the reason the loop cannot tell whether the
/// engine is in this process or on a machine across a socket. Two
/// implementations ship — [`Local`] and [`Remote`] — and they are the *same
/// code either side of a serialization boundary*: both go through [`serve`] to
/// produce a [`RunReport`] and [`RunReport::into_ran`] to read it. There is no
/// second path that could drift.
#[async_trait]
pub trait Runner: Send + Sync {
    /// Run one attempt. Never fails — see the module note.
    async fn run(&self, attempt: &Attempt) -> Ran;
}

/// Run the graph in this process.
pub struct Local<'a> {
    /// The real capabilities: agents, tools, HTTP, code.
    pub caps: &'a Capabilities,
    /// What can say whether anything changed.
    pub workspace: &'a dyn Workspace,
}

#[async_trait]
impl Runner for Local<'_> {
    async fn run(&self, attempt: &Attempt) -> Ran {
        run_attempt(attempt, self.caps, self.workspace).await
    }
}

/// Carrying a request to an engine somewhere else, and a report back.
///
/// The crate owns the contract and not the transport: Socket.IO, HTTP, a queue
/// and a unix socket are all the host's business. An implementation is expected
/// to apply its own deadline and return `Err` when it expires — [`Remote`]
/// treats that as an attempt, not as an exception.
#[async_trait]
pub trait Relay: Send + Sync {
    /// Send `request` and wait for the matching report.
    ///
    /// # Errors
    /// Whatever the transport calls a failure: no runner connected, a deadline,
    /// a malformed reply. The string is recorded, so make it readable.
    async fn dispatch(&self, request: &RunRequest) -> Result<RunReport, String>;
}

/// Run the graph somewhere else, over a [`Relay`].
pub struct Remote<'a> {
    /// The transport.
    pub relay: &'a dyn Relay,
    /// Correlates request and reply, and appears in the ledger row.
    pub attempt_id: String,
}

#[async_trait]
impl Runner for Remote<'_> {
    async fn run(&self, attempt: &Attempt) -> Ran {
        let request = RunRequest {
            attempt_id: self.attempt_id.clone(),
            graph: attempt.graph.clone(),
            inputs: attempt.inputs.clone(),
        };
        match self.relay.dispatch(&request).await {
            Ok(report) => report.into_ran(&attempt.graph),
            Err(why) => unreported(&attempt.graph, &why),
        }
    }
}

/// What a runner does when it receives a [`RunRequest`].
///
/// The far side of [`Remote`], and the whole of [`Local`]. A host embedding the
/// engine on a device calls this and sends the result back; a host running the
/// engine in-process gets the identical value without a wire.
pub async fn serve(
    request: &RunRequest,
    caps: &Capabilities,
    workspace: &dyn Workspace,
) -> RunReport {
    let mark = workspace.mark().await;
    let (capture, observer) = capturing();

    let failure = match compile(&request.graph) {
        Ok(compiled) => {
            let input = RunInput::new(json!({})).with_inputs(request.inputs.clone());
            match run_with_observer(&compiled, input, caps, &observer).await {
                Ok(outcome) => {
                    return report(request, &capture, workspace, &mark, None, outcome).await;
                }
                Err(err) => err.to_string(),
            }
        }
        // Nothing ran, so there are no steps — and `diagnose` against an empty
        // step list reports every node as never-reached, which is exactly true.
        Err(err) => err.to_string(),
    };

    let empty = RunOutcome {
        output: json!({}),
        pending_approvals: Vec::new(),
        cancelled: false,
    };
    report(request, &capture, workspace, &mark, Some(failure), empty).await
}

/// Assemble the report, reading the workspace last.
///
/// The reading happens after the run either way: a run that errored half way
/// through still wrote whatever it wrote before it did, and that is often the
/// only thing distinguishing "it broke" from "it broke having already done the
/// work".
async fn report(
    request: &RunRequest,
    capture: &Arc<tinyflows::diagnostics::CapturingObserver>,
    workspace: &dyn Workspace,
    mark: &str,
    failed: Option<String>,
    outcome: RunOutcome,
) -> RunReport {
    RunReport {
        attempt_id: request.attempt_id.clone(),
        steps: capture
            .steps()
            .iter()
            .map(|step| StepRecord::bounded(step, RECORD_BUDGET))
            .collect(),
        pending_approvals: outcome.pending_approvals,
        cancelled: outcome.cancelled,
        changed: workspace.changed_since(mark).await,
        failed,
        // Not measurable from here. A host that meters its harness fills this
        // in on the report before sending it.
        cost_usd: 0.0,
    }
}

/// Compile and run one attempt in this process, observed.
///
/// Never fails. Compilation errors, validation errors and mid-run failures all
/// come back as a [`Ran`] with `failed` set — see the module note: an attempt
/// that produced no ledger row is an attempt the next pass repeats.
pub async fn run_attempt(attempt: &Attempt, caps: &Capabilities, workspace: &dyn Workspace) -> Ran {
    let request = RunRequest {
        attempt_id: String::new(),
        graph: attempt.graph.clone(),
        inputs: attempt.inputs.clone(),
    };
    serve(&request, caps, workspace)
        .await
        .into_ran(&attempt.graph)
}

/// The reply that never came.
///
/// Deliberately *not* an empty `changed`. Empty means "the host looked and saw
/// nothing"; here nobody looked, and the difference decides the episode.
///
/// A run with no steps and an empty `changed` is settled mechanically as
/// [`crate::contracts::Blocker::MissingEvidence`], which is **terminal** — the
/// reasoning being that a retry with the same inputs produces the same nothing.
/// That reasoning is right for a graph that did nothing and wrong for a device
/// that dropped off: `ExternalWait` is terminal too, so either would strand the
/// episode permanently because a socket blipped. Saying plainly that the result
/// is unknown routes it to the judge, which can reach a continuable verdict.
fn unreported(graph: &tinyflows::model::WorkflowGraph, why: &str) -> Ran {
    RunReport {
        changed: format!(
            "unknown — the runner did not report ({why}). Whether the run did any \
             of the work is not established either way."
        ),
        failed: Some(format!("no report from the runner: {why}")),
        ..RunReport::default()
    }
    .into_ran(graph)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Git(&'static str);

    #[async_trait]
    impl Workspace for Git {
        async fn mark(&self) -> String {
            "abc123".into()
        }
        async fn changed_since(&self, mark: &str) -> String {
            format!("{} since {mark}", self.0)
        }
    }

    #[tokio::test]
    async fn a_host_that_cannot_say_reports_nothing_rather_than_guessing() {
        let quiet = Unobserved;
        assert!(quiet.mark().await.is_empty());
        assert!(quiet.changed_since("").await.is_empty());
    }

    #[tokio::test]
    async fn the_baseline_is_passed_back_to_the_comparison() {
        // The reason this is a trait and not a closure: the mark taken before
        // the run has to reach the reading taken after it.
        let git = Git("1 file changed");
        let mark = git.mark().await;
        assert_eq!(
            git.changed_since(&mark).await,
            "1 file changed since abc123"
        );
    }

    fn bare_graph() -> tinyflows::model::WorkflowGraph {
        tinyflows::model::WorkflowGraph {
            schema_version: 1,
            id: Some("g".into()),
            name: "g".into(),
            inputs: Vec::new(),
            agents: Vec::new(),
            nodes: Vec::new(),
            edges: Vec::new(),
        }
    }

    #[test]
    fn a_failure_is_readable_as_evidence_not_as_an_absence() {
        let ran = RunReport {
            failed: Some("node 'fetch' timed out".into()),
            ..RunReport::default()
        }
        .into_ran(&bare_graph());
        let evidence = ran.evidence();
        assert_eq!(
            evidence.outcome.output["error"],
            json!("node 'fetch' timed out")
        );
        // No `nodes` key: what the mechanical missing-evidence check reads.
        assert!(evidence.outcome.output.get("nodes").is_none());
    }

    #[test]
    fn an_unreported_run_does_not_claim_nothing_changed() {
        // The bug this exists to prevent. Empty `changed` plus no steps is
        // settled mechanically as MissingEvidence, which is terminal — so a
        // socket blip would end the episode for good. `ExternalWait` is
        // terminal too, so there is no safe blocker to pick; the fix is to stop
        // asserting a fact nobody established.
        let ran = unreported(&bare_graph(), "deadline elapsed after 600s");

        assert!(
            !ran.changed.is_empty(),
            "empty means the host looked and saw nothing; nobody looked"
        );
        assert!(ran.changed.contains("unknown"), "{}", ran.changed);
        assert!(
            ran.failed
                .as_deref()
                .unwrap_or_default()
                .contains("deadline"),
            "the transport's own words survive: {:?}",
            ran.failed
        );
    }

    #[test]
    fn an_unreported_run_carries_no_invented_evidence() {
        let ran = unreported(&bare_graph(), "no runner connected");
        assert!(ran.steps.is_empty());
        assert!(ran.outcome.pending_approvals.is_empty());
        assert!(!ran.outcome.cancelled);
        assert!(ran.outcome.output.get("nodes").is_none());
    }
}
