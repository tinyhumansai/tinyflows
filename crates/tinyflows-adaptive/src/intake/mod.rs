//! Prompt in, runnable graph out.
//!
//! Two paths and one rule between them: **prefer a stored workflow, author only
//! when nothing fits.** That ordering is the whole economic argument of the
//! loop — a procedure that has already worked costs one cheap selection call to
//! reuse and a full authoring call to reinvent, and reinventing it also throws
//! away every score it had accumulated.
//!
//! Neither path names a model or a provider. Both reach inference through the
//! engine's own [`LlmProvider`](tinyflows::caps::LlmProvider), so the host decides who answers and supplies
//! the credential as an opaque `conn` reference this crate never inspects.
//!
//! What comes out is an [`Attempt`]: an [`Approach`] saying how the decision was
//! reached, a graph that has been **validated**, and the inputs to run it with.
//! A graph leaves here compilable or not at all.

mod author;
pub mod recipe;
mod select;

pub use author::author;
pub use recipe::{Callable, render_callables};
pub use select::{Candidate, bind, select};

use serde_json::{Map, Value};
use tinyflows::caps::Capabilities;
use tinyflows::model::WorkflowGraph;
use tinyflows::store::{WorkflowStore, WorkflowSummary};
use tinyflows::validate::validate_all;

use crate::contracts::{Approach, Goal};
use crate::host::HostFacts;
use crate::ledger::Ledger;

/// What intake decided to run, and how it got there.
#[derive(Debug, Clone)]
pub struct Attempt {
    /// Selected, authored, or a variant — and why. Becomes the ledger row's
    /// signature, and therefore the next attempt's exclusion list.
    pub approach: Approach,
    /// Validated. An invalid graph is an error from intake, never a return
    /// value: handing one to the engine turns an authoring mistake into a
    /// run-time failure that looks like the work failing.
    pub graph: WorkflowGraph,
    /// Values for the graph's declared inputs, by name.
    pub inputs: Map<String, Value>,
    /// Continue a previous run rather than starting this graph from its
    /// trigger.
    ///
    /// Set only by the loop, only after a repair whose edits left the failed
    /// node's whole upstream alone. A [`Runner`](crate::execute::Runner) that
    /// sees `Some` re-enters the named node on the committed prefix; one that
    /// cannot must run the graph normally rather than fail — continuing is an
    /// optimisation over a correctness floor, never a requirement.
    pub resume: Option<crate::contracts::ResumePoint>,
    /// The lessons this attempt's planner was shown.
    ///
    /// Carried so the closing pass can score them against what happened. A
    /// lesson's `applied` counter is the denominator of its help rate, and
    /// nothing was incrementing it: `score_lesson` had exactly one caller, the
    /// corroboration loop, which moves both numbers together. So every lesson
    /// read either 0/0 or n/n, the rate carried no information, and the
    /// ordering built on it could not order anything.
    pub lessons_shown: Vec<String>,
}

/// What went wrong deciding.
#[derive(Debug, thiserror::Error)]
pub enum IntakeError {
    /// The store could not be read.
    #[error("workflow store: {0}")]
    Store(String),
    /// The ledger could not be read.
    #[error("ledger: {0}")]
    Ledger(#[from] crate::ledger::LedgerError),
    /// The model was unreachable, or answered with something unusable.
    #[error("inference: {0}")]
    Inference(String),
    /// The model authored a graph the engine would refuse.
    #[error("authored an invalid graph: {0}")]
    Invalid(String),
    /// The graph is well formed but names something this host does not have —
    /// a worker, a tool slug, a reachable address. Distinct from `Invalid`
    /// because the graph is fine and the *machine* is the constraint, which is
    /// what the retry has to be told.
    #[error("this host cannot run that graph: {0}")]
    Unsupported(String),
    /// A stored workflow was chosen whose declared inputs cannot be filled.
    #[error("workflow {id} needs an input nothing supplied: {missing}")]
    Unbindable {
        /// The workflow that could not be bound.
        id: String,
        /// The first input with no value.
        missing: String,
    },
}

/// Convenience alias for intake results.
pub type Result<T> = std::result::Result<T, IntakeError>;

/// Decide how to attempt `goal`, given what this episode has already tried.
///
/// Selection runs first and authoring is the fallback, not the default. The
/// exclusion list matters more here than anywhere else in the loop: without it
/// attempt four re-selects the workflow attempt two already failed on, and the
/// episode pays twice for one dead end.
///
/// # Errors
/// When the store or ledger cannot be read, inference fails, or the authored
/// graph does not validate.
pub async fn decide(
    goal: &Goal,
    episode: &str,
    store: &dyn WorkflowStore,
    ledger: &dyn Ledger,
    facts: &HostFacts,
    caps: &Capabilities,
    conn: Option<&str>,
) -> Result<Attempt> {
    // One read, two uses. The exclusion list and the rendered history are the
    // same rows seen two ways, and `Ledger::tried` is a fresh query — calling
    // it here as well would pay for the identical result twice on every
    // attempt, against whatever database the host brought.
    let rows = ledger.rows(episode).await?;
    let tried = crate::ledger::signatures(&rows);
    // One listing, two views of the same shelf, and the difference is the
    // point. The chooser sees what it has not already tried, because repeating
    // a selection cannot teach the episode anything. The author sees the WHOLE
    // shelf, because a workflow that fell short as the entire answer is exactly
    // the one worth calling as one step of a bigger plan.
    //
    // Listed once for the same reason `ledger.rows` is read once above: against
    // a file-backed store a second `list` is a second directory scan and a
    // second parse of every record, paid on every attempt for a result already
    // in hand.
    let listed = store
        .list()
        .map_err(|e| IntakeError::Store(e.to_string()))?;
    let candidates = catalogue(&listed, ledger, &tried).await?;
    let callables = callables(&listed);

    // Both planners see the same past, in the same words. The exclusion list
    // stops a *selection* being repeated, but nothing structural stops the
    // author writing attempt two's graph again on attempt four — only being
    // shown attempt two does. And the lessons were being written and never
    // read, which is a knowledge store that costs money and returns nothing.
    let lessons = crate::recall::retrieve(
        ledger.lessons(None).await?,
        None,
        crate::recall::RECALL_LIMIT,
    );
    let past = format!(
        "{}{}",
        crate::recall::render_history(&rows),
        crate::recall::render_lessons(&lessons)
    );

    let shown: Vec<String> = lessons.iter().map(|l| l.id.clone()).collect();

    // One errand per episode. A goal answered in a turn is an errand; a goal
    // that took a turn and is still not done was never one, and offering the
    // option again would let an episode spend its whole budget on single turns
    // that each fall the same way short.
    let errand_allowed = !tried.iter().any(|sig| sig == "errand");

    if let Some(chosen) = select(goal, &candidates, &past, errand_allowed, caps, conn).await? {
        // An errand names no stored workflow, so there is nothing to bind and
        // nothing to load: its graph is lowered from the goal itself.
        if matches!(chosen.approach, Approach::Errand { .. }) {
            return errand_attempt(goal, chosen, facts, shown);
        }
        // `select` answers with an id; the graph and the input check come from
        // the store. Returning the choice unbound would hand the engine an
        // empty graph, which compiles to nothing and reads as the work failing.
        match bind(chosen, store) {
            Ok(attempt) => {
                return Ok(Attempt {
                    lessons_shown: shown,
                    ..attempt
                });
            }
            Err(refusal @ IntakeError::Unbindable { .. }) => {
                // A sound choice that failed to bind — the model asserted
                // inputs it did not supply. That is a correctable slip, not a
                // reason to end the episode: one more selection round with
                // the refusal on the table, and authoring after that, because
                // authoring can always produce something runnable.
                let noted = format!(
                    "{past}\n\n# A selection just failed to bind\n{refusal}\n\
                     Supply a value for every required input this time, or \
                     decline so a graph is written instead."
                );
                // No errand on the retry: the question was asked and answered
                // one call ago, and this round exists to fix a binding slip.
                // Re-offering it would let a model that could not fill an input
                // reach for the one answer that needs none.
                if let Some(retry) = select(goal, &candidates, &noted, false, caps, conn).await? {
                    match bind(retry, store) {
                        Ok(attempt) => {
                            return Ok(Attempt {
                                lessons_shown: shown,
                                ..attempt
                            });
                        }
                        // Still the model's slip: authoring takes over below.
                        Err(IntakeError::Unbindable { .. }) => {}
                        // The store failing mid-decision is not something
                        // another model call can talk its way around.
                        Err(err) => return Err(err),
                    }
                }
                return author(goal, facts, &callables, store.policy(), &noted, caps, conn)
                    .await
                    .map(|attempt| Attempt {
                        lessons_shown: shown,
                        ..attempt
                    });
            }
            // Everything else — a store read failure, a vanished record — is
            // infrastructure, not a refusal: retrying selection would spend
            // model calls to run an authored graph against a store that is
            // not answering.
            Err(err) => return Err(err),
        }
    }
    author(goal, facts, &callables, store.policy(), &past, caps, conn)
        .await
        .map(|attempt| Attempt {
            lessons_shown: shown,
            ..attempt
        })
}

/// Finish an errand: lower the one-step graph and hold it to the same gates.
///
/// Both gates matter here even though no model wrote the graph. `validate_all`
/// is free and catches a lowering that stopped producing a runnable shape.
/// `HostFacts::check` is the one that earns its place: an errand is an agent
/// turn, and a host with no agent capability must refuse it as
/// [`Unsupported`](IntakeError::Unsupported) — the same answer authoring would
/// give — rather than emit a graph the runner will fail on and the judge will
/// then blame on the work.
fn errand_attempt(
    goal: &Goal,
    chosen: Attempt,
    facts: &HostFacts,
    shown: Vec<String>,
) -> Result<Attempt> {
    let graph = recipe::errand(&goal.text)?;

    let problems = validate_all(&graph);
    if !problems.is_empty() {
        return Err(IntakeError::Invalid(
            problems
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; "),
        ));
    }
    let refused = facts.check(&graph);
    if !refused.is_empty() {
        return Err(IntakeError::Unsupported(refused.join("; ")));
    }

    Ok(Attempt {
        graph,
        lessons_shown: shown,
        ..chosen
    })
}

/// The stored workflows worth offering, with what is known about each.
///
/// Three filters, each removing something a planner must not be shown:
///
/// * **disabled** — the operator turned it off; offering it invites a choice
///   that cannot be honoured.
/// * **already tried this episode** — its signature is in the exclusion list.
/// * **not selectable** — a `draft` variant is a proposal, not a procedure. It
///   is run deliberately by whoever proposed it, never chosen by a planner that
///   has not seen its evidence.
///
/// The scores come from our ledger rather than the record, because
/// `WorkflowRecord` has no place for them: a score is a fact that spans runs.
///
/// Then one more pass: a repaired family collapses to a single row, its
/// [`champion`]. Four near-identical graphs whose descriptions differ by a
/// clause is not a choice, it is noise, and a planner asked to make it is being
/// asked to guess. Which member survives is decided on score, never on being
/// the newest — see [`crate::promotion`].
/// Every enabled workflow, as something a plan may call.
///
/// Unfiltered on purpose — see the note at the call site. Takes the listing the
/// chooser's catalogue already read, so composition costs no extra store
/// traffic: `WorkflowSummary` already carries the declared inputs, precisely
/// so a caller need not fetch a whole graph to learn what it takes.
fn callables(listed: &[WorkflowSummary]) -> Vec<Callable> {
    listed
        .iter()
        .filter(|summary| summary.enabled)
        .map(|summary| Callable {
            id: summary.id.clone(),
            name: summary.name.clone(),
            description: summary.description.clone(),
            inputs: declared_inputs(summary),
        })
        .collect()
}

/// A summary's declared inputs as the `(name, required)` pairs both prompts
/// render. One mapping, because two copies of it is how the chooser and the
/// author come to disagree about which inputs a workflow demands.
fn declared_inputs(summary: &WorkflowSummary) -> Vec<(String, bool)> {
    summary
        .inputs
        .iter()
        .map(|input| (input.name.clone(), input.required))
        .collect()
}

async fn catalogue(
    listed: &[WorkflowSummary],
    ledger: &dyn Ledger,
    tried: &[String],
) -> Result<Vec<Candidate>> {
    let mut out = Vec::new();
    for summary in listed {
        if !summary.enabled {
            continue;
        }
        let signature = format!("selected:{}", summary.id);
        if tried.iter().any(|t| t == &signature) {
            continue;
        }
        let score = ledger.workflow_score(&summary.id).await?;
        out.push(Candidate {
            id: summary.id.clone(),
            name: summary.name.clone(),
            description: summary.description.clone(),
            node_count: summary.node_count,
            applied: score.applied,
            helped: score.helped,
            inputs: declared_inputs(summary),
        });
    }
    collapse_families(out, ledger).await
}

/// Reduce each repaired family to its champion.
///
/// A member excluded earlier — disabled, or already tried this episode — is
/// still counted when picking the champion but cannot be the one offered. That
/// matters: if the champion is the workflow this episode just failed with,
/// dropping the whole family would hide a variant that exists precisely because
/// the champion fell short. So the family's *best still-offerable* member is
/// what survives.
async fn collapse_families(
    candidates: Vec<Candidate>,
    ledger: &dyn Ledger,
) -> Result<Vec<Candidate>> {
    let mut kept: Vec<Candidate> = Vec::new();
    let mut settled: Vec<String> = Vec::new();

    for candidate in candidates.iter() {
        if settled.contains(&candidate.id) {
            continue;
        }
        let lineage = ledger.lineage(&candidate.id).await?;
        if lineage.len() <= 1 {
            kept.push(candidate.clone());
            settled.push(candidate.id.clone());
            continue;
        }

        // Scores for the whole family, including members not on offer — a
        // parent that is disabled still counts as evidence about its variants.
        let mut family = Vec::with_capacity(lineage.len());
        for id in &lineage {
            family.push((id.clone(), ledger.workflow_score(id).await?));
        }
        let best = crate::promotion::champion(&family).unwrap_or(&candidate.id);

        let offer = candidates
            .iter()
            .find(|c| c.id == best)
            .or_else(|| {
                // The champion is not offerable. Fall back to the best of what
                // is, in family order, rather than dropping the family whole.
                lineage
                    .iter()
                    .find_map(|id| candidates.iter().find(|c| &c.id == id))
            })
            .unwrap_or(candidate);
        if !settled.contains(&offer.id) {
            kept.push(offer.clone());
        }
        settled.extend(lineage);
    }
    Ok(kept)
}

/// Ask the host's model for one JSON object.
///
/// Every intake call has this shape, and the failure modes are shared: a model
/// that answers with prose around its JSON, or with nothing. Both become
/// [`IntakeError::Inference`] here rather than at three call sites.
///
/// # Errors
/// When the provider fails, or its answer holds no JSON object.
pub(crate) async fn ask(
    caps: &Capabilities,
    conn: Option<&str>,
    tier: crate::contracts::Tier,
    system: &str,
    user: &str,
) -> Result<Value> {
    let request = serde_json::json!({
        // Which job, never which model. A host reads this to route judging and
        // selecting to different places; one that ignores it gets the old
        // behaviour, which is why it is a plain field and not a required one.
        "tier": tier.as_str(),
        "messages": [
            { "role": "system", "content": system },
            { "role": "user", "content": user },
        ],
        // A hint, not a guarantee: hosts differ in whether they honour it, so
        // `extract` still has to cope with prose around the object.
        "response_format": { "type": "json_object" },
    });

    let answer = caps
        .llm
        .complete(request, conn)
        .await
        .map_err(|e| IntakeError::Inference(e.to_string()))?;

    extract(&answer).ok_or_else(|| {
        IntakeError::Inference(format!("no JSON object in the reply: {}", peek(&answer)))
    })
}

/// The JSON object inside a completion response, wherever the host put it.
///
/// Hosts wrap differently — some return the object, some an OpenAI-shaped
/// envelope, some a string of JSON in a `text` field. Rather than demand one
/// shape from every host, this reads all three, because the alternative is a
/// crate that only works against the provider it was written for.
fn extract(answer: &Value) -> Option<Value> {
    if answer.is_object() && !answer["choices"].is_array() && answer.get("text").is_none() {
        return Some(answer.clone());
    }
    let text = answer["choices"][0]["message"]["content"]
        .as_str()
        .or_else(|| answer["text"].as_str())
        .or_else(|| answer["content"].as_str())?;
    from_text(text)
}

/// A JSON object out of text that may have prose around it.
fn from_text(text: &str) -> Option<Value> {
    if let Ok(value) = serde_json::from_str::<Value>(text.trim()) {
        return Some(value);
    }
    // A fenced block, or a sentence before the object. Bounded by the first `{`
    // and the last `}` rather than by parsing markdown, which a model will
    // eventually emit in a form no parser expected.
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    serde_json::from_str(text.get(start..=end)?).ok()
}

fn peek(value: &Value) -> String {
    let mut text = value.to_string();
    // Floor to a char boundary: `truncate` panics mid-codepoint, and this runs
    // on exactly the path that should become an `Inference` error — a provider
    // reply with a multi-byte character at byte 200 must not abort the task.
    let mut end = text.len().min(200);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_object_is_read_as_itself() {
        let answer = serde_json::json!({ "workflow_id": "pr-review" });
        assert_eq!(extract(&answer).unwrap()["workflow_id"], "pr-review");
    }

    #[test]
    fn an_openai_shaped_envelope_is_unwrapped() {
        let answer = serde_json::json!({
            "choices": [{ "message": { "content": "{\"workflow_id\":\"pr-review\"}" } }]
        });
        assert_eq!(extract(&answer).unwrap()["workflow_id"], "pr-review");
    }

    #[test]
    fn a_text_field_holding_json_is_read() {
        let answer = serde_json::json!({ "text": "{\"workflow_id\":\"x\"}" });
        assert_eq!(extract(&answer).unwrap()["workflow_id"], "x");
    }

    #[test]
    fn prose_around_the_object_does_not_lose_it() {
        // Models do this whatever the response_format asked for.
        let answer = serde_json::json!({
            "text": "Sure! Here you go:\n```json\n{\"workflow_id\":\"x\"}\n```\nHope that helps."
        });
        assert_eq!(extract(&answer).unwrap()["workflow_id"], "x");
    }

    #[test]
    fn an_answer_with_no_object_at_all_is_none_rather_than_a_panic() {
        assert!(extract(&serde_json::json!({ "text": "I could not decide." })).is_none());
        assert!(extract(&serde_json::json!("just a string")).is_none());
    }
}
