//! Choosing a stored workflow, or declining to.
//!
//! The cheap path, and the one that should win once anything has been learned.
//! A selection is one small call against a list; authoring is a large call that
//! also discards whatever the existing procedure had proved about itself.
//!
//! Declining is a first-class answer, not a failure. A model pushed to always
//! pick something will pick the nearest thing, and a near-miss workflow runs to
//! completion producing confidently wrong work — which is more expensive than
//! authoring, not less.

use serde_json::{Map, Value};
use tinyflows::caps::Capabilities;
use tinyflows::model::WorkflowGraph;
use tinyflows::store::WorkflowStore;

use super::{Attempt, IntakeError, Result, ask};
use crate::contracts::{Approach, Goal, Tier};

/// One stored workflow as the chooser sees it.
#[derive(Debug, Clone)]
pub struct Candidate {
    /// The id the choice is made on.
    pub id: String,
    /// Display name; falls back to the id when blank.
    pub name: String,
    /// What the model actually reads to decide. A workflow with none is a row
    /// nobody can choose on purpose.
    pub description: String,
    /// A rough cost signal.
    pub node_count: usize,
    /// Times chosen and run.
    pub applied: u32,
    /// Times that ended satisfied.
    pub helped: u32,
    /// Its declared inputs: name and whether it is required.
    ///
    /// Listed because the chooser is asked to supply values for them. It was
    /// being asked to fill inputs it had never been shown, which is a guess
    /// dressed as a binding — and a required input guessed wrong is a run
    /// that fails after the choice has already been made.
    pub inputs: Vec<(String, bool)>,
}

impl Candidate {
    fn render(&self) -> String {
        let name = if self.name.is_empty() {
            &self.id
        } else {
            &self.name
        };
        let description = if self.description.is_empty() {
            "(no description — nobody can choose this on purpose)"
        } else {
            &self.description
        };
        // Both numbers, never a rate: 1/1 and 40/40 are the same rate and are
        // not the same evidence, and the model is being asked to weigh exactly
        // that difference.
        let record = match self.applied {
            0 => "never run".to_string(),
            applied => format!("run {applied}×, satisfied {}×", self.helped),
        };
        let inputs = if self.inputs.is_empty() {
            String::new()
        } else {
            format!("\n  inputs: {}", super::recipe::render_inputs(&self.inputs))
        };
        format!(
            "- id: {}\n  name: {name}\n  steps: {}, {record}{inputs}\n  {description}",
            self.id, self.node_count
        )
    }
}

const SYSTEM: &str = "\
You choose whether a saved workflow already does what a goal asks for.

Return JSON: {\"workflow_id\": str | null, \"errand\": bool, \"why\": str,
              \"inputs\": {name: value}}

- workflow_id: the id of the workflow that does this, or null.
- errand: true only when the goal is one turn of work with no procedure in it.
- why: one line. When you decline, say what is missing — it is read by whoever
  writes the replacement.
- inputs: values for that workflow's declared inputs, taken from the goal. Only
  what the goal actually states; never invent a repository, a path or an id.

Choose one ONLY when it does what the goal asks. A workflow that does something
adjacent is worse than none: it will run to completion and produce confident
work for a job nobody wanted, which costs more than writing a new one.

Prefer a workflow with a record over one without, and weigh both numbers rather
than the ratio — run 40× satisfied 30× is a known quantity, run 1× satisfied 1×
is a coin landing once. A workflow that has never run is still a fair choice
when it plainly matches; it just carries no evidence.

When this episode has already tried something, decline rather than choose a
workflow that would fall short the same way. Being told a second time that the
report has no numbers in it costs a full run and establishes nothing.

Set errand only when there is no procedure in the goal — one turn of work,
answered and finished, with nothing a later goal would want to reuse. Ask
whether you would want this written down and offered as a choice next month.

  errand   \"how much disk is this directory using\"
  errand   \"what did the last commit change\"
  NOT      \"summarise a paper into three bullets\"  — one step, and exactly the
           kind of thing worth having on the shelf
  NOT      \"check the PR and fix whatever CI says\"  — one sentence, many turns

Short is not the test, and a single step is not the test: a one-step procedure
can be the most reused thing here. The test is whether a *procedure* exists.

An errand is not an escape from a hard goal. Anything that needs several turns,
or that could fail in a way worth retrying differently, is not one — say so and
decline instead, so a graph gets written.";

/// Ask whether any candidate does the job, and bind its inputs if one does.
///
/// `Ok(None)` means nothing fitted — the ordinary case on a cold store, and the
/// caller's cue to author. `Ok(Some)` is either a
/// [`Selected`](Approach::Selected) whose graph the caller loads from the store,
/// or an [`Errand`](Approach::Errand) whose graph the caller lowers; both come
/// back with an empty graph, because what fills it is not this function's job.
///
/// `errand_allowed` is false once this episode has already spent its errand —
/// see [`Approach::signature`]. Withholding the option is structural rather
/// than left to the prompt, because "you already tried that" is exactly the
/// instruction a model talks itself out of on attempt three.
///
/// # Errors
/// When inference fails, or the chosen workflow cannot be loaded or bound.
pub async fn select(
    goal: &Goal,
    candidates: &[Candidate],
    past: &str,
    errand_allowed: bool,
    caps: &Capabilities,
    conn: Option<&str>,
) -> Result<Option<Attempt>> {
    // With nothing to choose from and no errand to offer, the answer can only
    // be "none" and asking costs a call to be told so.
    //
    // This used to be unconditional, on that reasoning — and the reasoning
    // stopped holding the moment there was a third answer. A cold store is
    // precisely where a trivial goal is most likely, so short-circuiting here
    // would have made the errand path unreachable exactly where it pays most,
    // while looking correct. The cost is honest and worth stating: a cold-store
    // episode that is *not* an errand now pays one small extra call, against
    // saving a full authoring call and its run whenever it is.
    if candidates.is_empty() && !errand_allowed {
        return Ok(None);
    }

    let shelf = if candidates.is_empty() {
        "# Saved workflows\n(none yet — nothing to choose from, so the only \
         question is whether this is an errand)"
            .to_string()
    } else {
        format!(
            "# Saved workflows\n{}",
            candidates
                .iter()
                .map(Candidate::render)
                .collect::<Vec<_>>()
                .join("\n")
        )
    };
    let spent = if errand_allowed {
        String::new()
    } else {
        "\n\n# This episode has already spent its errand\nOne turn was tried \
         and did not finish the goal, so it is not an errand. Choose a workflow \
         or decline; `errand` will be ignored."
            .to_string()
    };
    let user = format!("# Goal\n{}\n\n{shelf}{past}{spent}", goal.text.trim());

    let answer = ask(caps, conn, Tier::Select, SYSTEM, &user).await?;
    let Some(id) = answer["workflow_id"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
    else {
        // Declining and calling it an errand are different answers, and only
        // one of them skips authoring. Read second so a model that names a
        // workflow *and* sets the flag is taken at its first word — the
        // workflow is the more specific claim, and the more easily checked.
        if errand_allowed && answer["errand"].as_bool().unwrap_or(false) {
            return Ok(Some(Attempt {
                approach: Approach::Errand {
                    why: answer["why"].as_str().unwrap_or_default().to_string(),
                },
                // Lowered by the caller, which is what holds the host facts the
                // one-step graph has to be checked against.
                graph: WorkflowGraph::default(),
                inputs: Map::new(),
                resume: None,
                lessons_shown: Vec::new(),
            }));
        }
        return Ok(None);
    };
    // A model naming something that is not on the list has hallucinated an id;
    // treat it as a decline rather than looking it up, or a typo becomes a
    // store read for a workflow nobody offered.
    if !candidates.iter().any(|c| c.id == id) {
        return Ok(None);
    }

    Ok(Some(Attempt {
        approach: Approach::Selected {
            workflow_id: id.to_string(),
            why: answer["why"].as_str().unwrap_or_default().to_string(),
        },
        graph: WorkflowGraph::default(),
        inputs: inputs_of(&answer),
        // Intake never continues a run: only the loop knows whether the
        // repair it just made is safe to skip a prefix over.
        resume: None,
        // Filled by `decide`, which is what knows what the planner was shown.
        lessons_shown: Vec::new(),
    }))
}

/// Load the chosen workflow and check every declared input has a value.
///
/// Binding is checked here, *after* the model picks and before anything runs.
/// The model is confident about inputs it did not actually find in the goal, so
/// the cheap deterministic check catches what the expensive one asserted.
///
/// The check runs in both directions. A required input the model did not
/// supply is an error. An input the model supplied that the graph never
/// declared is *dropped*: the engine rejects undeclared keys before any node
/// executes, so one invented key — and models invent them freely — would
/// otherwise turn a sound selection into an attempt that ran nothing.
///
/// # Errors
/// When the workflow is gone, or an input has no value.
pub fn bind(attempt: Attempt, store: &dyn WorkflowStore) -> Result<Attempt> {
    let Approach::Selected {
        ref workflow_id, ..
    } = attempt.approach
    else {
        return Ok(attempt);
    };
    let record = store
        .get(workflow_id)
        .map_err(|e| IntakeError::Store(e.to_string()))?
        .ok_or_else(|| IntakeError::Store(format!("workflow {workflow_id} vanished")))?;

    for declared in &record.graph.inputs {
        if !declared.required {
            continue;
        }
        let filled = attempt
            .inputs
            .get(&declared.name)
            .is_some_and(|v| !v.is_null() && v.as_str() != Some(""));
        if !filled {
            return Err(IntakeError::Unbindable {
                id: workflow_id.clone(),
                missing: declared.name.clone(),
            });
        }
    }

    let mut attempt = attempt;
    attempt
        .inputs
        .retain(|name, _| record.graph.inputs.iter().any(|d| d.name == *name));

    Ok(Attempt {
        graph: record.graph,
        ..attempt
    })
}

fn inputs_of(answer: &Value) -> Map<String, Value> {
    answer["inputs"].as_object().cloned().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn candidate(id: &str, applied: u32, helped: u32) -> Candidate {
        Candidate {
            id: id.to_string(),
            name: format!("the {id} workflow"),
            description: "reviews a closed issue end to end".to_string(),
            node_count: 4,
            applied,
            helped,
            inputs: vec![("repo".to_string(), true)],
        }
    }

    #[test]
    fn a_listing_shows_both_counters_not_a_rate() {
        let rendered = candidate("pr-review", 40, 30).render();
        assert!(rendered.contains("run 40×, satisfied 30×"), "{rendered}");
        assert!(!rendered.contains("75"), "a rate hides the sample size");
    }

    #[test]
    fn a_workflow_that_has_never_run_says_so_rather_than_showing_zeroes() {
        let rendered = candidate("fresh", 0, 0).render();
        assert!(rendered.contains("never run"), "{rendered}");
    }

    #[test]
    fn a_workflow_with_no_description_says_it_cannot_be_chosen_on_purpose() {
        let mut c = candidate("bare", 0, 0);
        c.description = String::new();
        assert!(c.render().contains("nobody can choose this on purpose"));
    }

    #[test]
    fn a_blank_name_falls_back_to_the_id() {
        let mut c = candidate("only-an-id", 1, 1);
        c.name = String::new();
        assert!(c.render().contains("name: only-an-id"));
    }

    /// A model that answers with `reply` and records what it was shown.
    ///
    /// The call count is the point of several tests below: whether `select`
    /// asks at all is a cost decision, and asserting on the *answer* would not
    /// notice a version that skipped the call and returned the same thing.
    struct Scripted {
        reply: Value,
        asked: std::sync::Mutex<Vec<String>>,
    }

    impl Scripted {
        fn new(reply: Value) -> std::sync::Arc<Self> {
            std::sync::Arc::new(Self {
                reply,
                asked: std::sync::Mutex::new(Vec::new()),
            })
        }
        fn calls(&self) -> usize {
            self.asked.lock().expect("log").len()
        }
        fn last(&self) -> String {
            self.asked
                .lock()
                .expect("log")
                .last()
                .cloned()
                .unwrap_or_default()
        }
    }

    #[async_trait::async_trait]
    impl tinyflows::caps::LlmProvider for Scripted {
        async fn complete(
            &self,
            request: Value,
            _conn: Option<&str>,
        ) -> tinyflows::error::Result<Value> {
            self.asked.lock().expect("log").push(
                request["messages"][1]["content"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            );
            Ok(self.reply.clone())
        }
    }

    async fn choose(
        provider: &std::sync::Arc<Scripted>,
        candidates: &[Candidate],
        errand_allowed: bool,
    ) -> Option<Attempt> {
        let caps = Capabilities {
            llm: provider.clone(),
            ..tinyflows::caps::mock::mock_capabilities()
        };
        select(
            &Goal::new("how much disk is this directory using"),
            candidates,
            "",
            errand_allowed,
            &caps,
            None,
        )
        .await
        .expect("selection answers")
    }

    #[tokio::test]
    async fn an_errand_answer_becomes_an_errand_approach() {
        let provider = Scripted::new(json!({
            "workflow_id": null,
            "errand": true,
            "why": "one turn of work, no procedure in it",
        }));
        let chosen = choose(&provider, &[candidate("pr-review", 4, 4)], true)
            .await
            .expect("an errand is an answer, not a decline");
        match chosen.approach {
            Approach::Errand { why } => assert!(why.contains("one turn")),
            other => panic!("expected an errand, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_empty_shelf_is_still_asked_when_an_errand_is_possible() {
        // The bug this exists to prevent, and it would have been invisible: a
        // cold store is exactly where a trivial goal is most likely, so the old
        // unconditional short-circuit made the errand path unreachable at the
        // one moment it pays for itself — while every test of the *answer*
        // still passed.
        let provider =
            Scripted::new(json!({ "workflow_id": null, "errand": true, "why": "trivial" }));
        let chosen = choose(&provider, &[], true).await;
        assert_eq!(provider.calls(), 1, "an empty shelf must still be asked");
        assert!(matches!(
            chosen.map(|a| a.approach),
            Some(Approach::Errand { .. })
        ));
    }

    #[tokio::test]
    async fn an_empty_shelf_with_no_errand_left_is_not_asked_at_all() {
        // The other half: with nothing to choose from and no errand to offer,
        // the answer can only be "none", and the call is pure cost.
        let provider = Scripted::new(json!({ "workflow_id": null, "errand": true, "why": "x" }));
        assert!(choose(&provider, &[], false).await.is_none());
        assert_eq!(provider.calls(), 0, "nothing to ask about");
    }

    #[tokio::test]
    async fn a_spent_errand_is_refused_even_when_the_model_asks_for_one() {
        // Prompt-only enforcement is not enforcement: attempt three is exactly
        // where a model talks itself back into the answer that needs no inputs.
        let provider =
            Scripted::new(json!({ "workflow_id": null, "errand": true, "why": "again" }));
        let chosen = choose(&provider, &[candidate("pr-review", 4, 4)], false).await;
        assert!(chosen.is_none(), "a spent errand reads as a decline");
        assert!(
            provider.last().contains("already spent its errand"),
            "and the prompt says why: {}",
            provider.last()
        );
    }

    #[tokio::test]
    async fn naming_a_workflow_wins_over_also_setting_the_errand_flag() {
        // A contradictory answer taken at its more specific — and more easily
        // checked — word, rather than at whichever field is read first.
        let provider = Scripted::new(json!({
            "workflow_id": "pr-review",
            "errand": true,
            "why": "both",
            "inputs": { "repo": "openhuman" },
        }));
        let chosen = choose(&provider, &[candidate("pr-review", 4, 4)], true)
            .await
            .expect("an answer");
        assert!(matches!(chosen.approach, Approach::Selected { .. }));
    }

    #[test]
    fn the_prompt_refuses_to_treat_a_short_goal_as_an_errand() {
        // The distinction the whole triage turns on. A one-step procedure can
        // be the most reused thing in the store, so brevity must not be the
        // test — if this guidance goes, the flag starts eating the shelf.
        assert!(SYSTEM.contains("no procedure in the goal"));
        assert!(SYSTEM.contains("Short is not the test"));
        assert!(SYSTEM.contains("not an escape from a hard goal"));
    }

    #[test]
    fn the_prompt_tells_the_model_that_declining_is_allowed() {
        // The single most important line in it: a model pushed to always pick
        // will pick the nearest thing, and a near miss runs to completion.
        assert!(SYSTEM.contains("or null"));
        assert!(SYSTEM.contains("worse than none"));
    }
}
