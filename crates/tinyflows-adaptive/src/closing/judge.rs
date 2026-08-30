//! Deciding whether a run actually did the job.
//!
//! Two stages, and the order is the whole point. **Mechanical evidence first**:
//! the engine's own diagnosis of the run says, deterministically, that a
//! binding resolved to null or that half the graph never executed. Those are
//! facts, they cost nothing, and a model asked to weigh them will sometimes
//! decide the run went fine anyway.
//!
//! Only what mechanism cannot settle goes to a model — and it is shown the
//! diagnosis rather than asked to infer it.
//!
//! The judge is deliberately context-poor: goal, outcome, diagnosis. It does
//! not see the ledger, so it cannot propose what to try next, because it does
//! not know what has already been ruled out. That is the planner's job.

use tinyflows::caps::Capabilities;
use tinyflows::diagnostics::Diagnosis;
use tinyflows::engine::RunOutcome;
use tinyflows::evidence::bounded_evidence;

use crate::contracts::{Blocker, Goal, Tier, Verdict};
use crate::intake::{Result, ask};

const SYSTEM: &str = "\
You judge whether a workflow run achieved a goal.

Return JSON: {\"satisfied\": bool, \"blocker\": str, \"gap\": str,
              \"attributed_to\": str, \"advanced\": bool}

- satisfied: did the run achieve the goal. Not \"did it finish\" — a run can
  complete every node and achieve nothing.
- blocker: when not satisfied, one of
    goal_not_met     it tried and fell short. The ordinary case — INCLUDING a
                     run that died on its own mechanics: a miswired binding, a
                     bad command flag, a refused tool call. The graph can be
                     changed, so the episode can continue; say what broke in
                     the gap.
    unverified       something was produced but the evidence does not show it
                     working.
    missing_evidence nothing was produced AND another attempt would meet the
                     same nothing — the goal itself offers no evidence to
                     collect. NOT for mechanical failures; those are
                     goal_not_met.
    needs_input      a person has to answer something first — not \"the graph
                     failed to supply a value\", which is goal_not_met.
    external_wait    waiting on something outside this system.

  goal_not_met and unverified let the loop try again with a changed approach;
  the other three end the episode. Choose the terminal ones only when another
  attempt genuinely cannot help.
- gap: one line on what is still missing. It is read by whoever plans the next
  attempt, so name the missing thing, not the feeling.
- attributed_to: the node id that fell short, when the evidence says which.
- advanced: did this run get closer to the goal than the state before it.
  A run can fail and still advance — establishing what the problem is counts.
  A run that produced the same nothing as the last one did not.

Judge the EVIDENCE, not the run's own account of itself. A node reporting
success having written nothing is the failure this exists to catch, and the
diagnosis below is the engine's own reading of what the steps actually did.";

/// What the run left behind, as the judge sees it.
///
/// Assembled by the caller so the judge cannot reach for anything else: it gets
/// the outcome, the diagnosis, and nothing about history.
#[derive(Debug, Clone)]
pub struct Evidence<'a> {
    /// What the engine returned.
    pub outcome: &'a RunOutcome,
    /// The engine's own reading of the steps — the four things a green outcome
    /// hides.
    pub diagnosis: &'a Diagnosis,
    /// What changed outside the run state, when the host can say. A workspace
    /// diff, a list of files, whatever the host counts as proof. Empty is
    /// honest; a fabricated summary is not.
    pub changed: String,
    /// The runner's own report that the run broke — a node that errored, a
    /// script that exited nonzero, a deadline. `None` when the run completed
    /// on its own terms, whatever it achieved.
    ///
    /// Carried separately from the outcome because it decides something the
    /// model may not: whether another attempt is worth making. See
    /// [`judge`]'s downgrade.
    pub failed: Option<String>,
}

impl Evidence<'_> {
    /// The parts of the diagnosis worth a sentence each.
    ///
    /// `unverifiable` null bindings are dropped: the engine marks an expression
    /// it could not evaluate even in principle, and reporting those as findings
    /// buries the ones that are real.
    fn findings(&self) -> Vec<String> {
        let mut out = Vec::new();
        for binding in &self.diagnosis.null_bindings {
            if binding.unverifiable {
                continue;
            }
            let from = binding
                .reads_from
                .as_deref()
                .map_or(String::new(), |n| format!(", reading from `{n}`"));
            out.push(format!(
                "node `{}`: `{}` at {} resolved to null{from} — {}",
                binding.node_id, binding.expression, binding.location, binding.suggestion
            ));
        }
        for node in &self.diagnosis.empty_prompts {
            out.push(format!(
                "node `{node}`: dispatched an agent session with an empty prompt"
            ));
        }
        for hidden in &self.diagnosis.hidden_errors {
            out.push(format!(
                "node `{}`: errored, and its on_error policy swallowed it{}",
                hidden.node_id,
                hidden
                    .message
                    .as_deref()
                    .map_or(String::new(), |m| format!(" — {m}"))
            ));
        }
        for skipped in &self.diagnosis.never_ran {
            out.push(format!(
                "node `{}`: never ran{}",
                skipped.node_id,
                skipped
                    .routed_by
                    .as_deref()
                    .map_or(String::new(), |n| format!(", routed past by `{n}`"))
            ));
        }
        out
    }

    pub(super) fn render(&self) -> String {
        let findings = self.findings();
        let diagnosis = if findings.is_empty() {
            "the engine found nothing wrong with the steps".to_string()
        } else {
            findings.join("\n- ")
        };
        format!(
            "# Run outcome\n{}\n\n# What the engine's diagnosis found\n- {diagnosis}\n\n\
             # What changed outside the run\n{}",
            serde_json::to_string_pretty(&bounded_evidence(&self.outcome.output))
                .unwrap_or_else(|_| "(unreadable)".into()),
            if self.changed.is_empty() {
                "(nothing reported)"
            } else {
                &self.changed
            }
        )
    }
}

/// Judge a finished run.
///
/// Three outcomes are decided without a model at all, because they are facts
/// rather than judgements and paying for an opinion on a fact is how a loop
/// gets expensive:
///
/// * a parked approval is `needs_input`;
/// * a cancelled run is `external_wait` — it did not fail, it was stopped;
/// * a run that produced nothing *and* whose diagnosis says nothing ran is
///   `missing_evidence`, which is terminal, because a retry with the same
///   inputs produces the same nothing.
///
/// # Errors
/// When inference fails or answers with nothing usable.
pub async fn judge(
    goal: &Goal,
    evidence: &Evidence<'_>,
    caps: &Capabilities,
    conn: Option<&str>,
) -> Result<Verdict> {
    if let Some(settled) = without_a_model(evidence) {
        return Ok(settled);
    }

    let criteria = if goal.success_criteria.trim().is_empty() {
        String::new()
    } else {
        format!("\n\n# Done when\n{}", goal.success_criteria.trim())
    };
    let user = format!(
        "# Goal\n{}{criteria}\n\n{}",
        goal.text.trim(),
        evidence.render()
    );

    let answer = ask(caps, conn, Tier::Judge, SYSTEM, &user).await?;
    let satisfied = answer["satisfied"].as_bool().unwrap_or(false);
    let blocker = if satisfied {
        // A satisfied verdict has no blocker whatever the model wrote in the
        // field; the two disagreeing is a state nothing downstream can read.
        Blocker::None
    } else {
        let claimed = Blocker::parse(answer["blocker"].as_str().unwrap_or_default());
        // The one place the loop overrules the judge, and it does so on a
        // fact rather than an opinion: the RUNNER said the run broke. A
        // mechanical break is the most fixable thing an episode can hit —
        // rewrite the script, correct the flag — so calling it terminal
        // spends the remaining attempts on nothing. The prompt says this
        // too; saying it is not enough, because a model that misreads it
        // ends the episode and no later round can undo that.
        //
        // Needs-input and external-wait survive: both are terminal because
        // something OUTSIDE the loop must move, which a broken run does not
        // change.
        if evidence.failed.is_some()
            && !claimed.continuable()
            && !matches!(claimed, Blocker::NeedsInput | Blocker::ExternalWait)
        {
            Blocker::GoalNotMet
        } else {
            claimed
        }
    };
    Ok(Verdict {
        satisfied,
        blocker,
        gap: answer["gap"].as_str().unwrap_or_default().to_string(),
        attributed_to: answer["attributed_to"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        evidence: evidence.findings().join("; "),
        // Absent must not read as "made no progress" — that would stall a run
        // for a field the model simply did not write.
        advanced: answer["advanced"].as_bool().unwrap_or(true),
    })
}

/// The verdicts that are facts rather than opinions.
fn without_a_model(evidence: &Evidence<'_>) -> Option<Verdict> {
    let outcome = evidence.outcome;

    if !outcome.pending_approvals.is_empty() {
        return Some(Verdict {
            satisfied: false,
            blocker: Blocker::NeedsInput,
            gap: format!(
                "parked for approval at: {}",
                outcome.pending_approvals.join(", ")
            ),
            attributed_to: outcome
                .pending_approvals
                .first()
                .cloned()
                .unwrap_or_default(),
            evidence: String::new(),
            // It got as far as the gate. That is progress, and calling it a
            // stall would count a waiting run against the stall limit.
            advanced: true,
        });
    }

    if outcome.cancelled {
        return Some(Verdict {
            satisfied: false,
            blocker: Blocker::ExternalWait,
            gap: "the run was cancelled before it finished".to_string(),
            attributed_to: String::new(),
            evidence: String::new(),
            advanced: true,
        });
    }

    // Nothing ran and nothing changed. There is no judgement to make and no
    // second opinion worth buying.
    let nothing_ran = !evidence.diagnosis.never_ran.is_empty()
        && outcome
            .output
            .get("nodes")
            .is_none_or(|n| n.as_object().is_none_or(serde_json::Map::is_empty));
    if nothing_ran && evidence.changed.is_empty() {
        return Some(Verdict {
            satisfied: false,
            blocker: Blocker::MissingEvidence,
            gap: "no node produced anything and nothing changed outside the run".to_string(),
            attributed_to: String::new(),
            evidence: String::new(),
            advanced: false,
        });
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A provider that answers the judge with a fixed blocker.
    struct Says(&'static str);

    #[async_trait::async_trait]
    impl tinyflows::caps::LlmProvider for Says {
        async fn complete(
            &self,
            _request: serde_json::Value,
            _conn: Option<&str>,
        ) -> tinyflows::error::Result<serde_json::Value> {
            Ok(serde_json::json!({
                "satisfied": false,
                "blocker": self.0,
                "gap": "nothing was fetched",
            }))
        }
    }

    async fn verdict_for(blocker: &'static str, failed: Option<String>) -> Verdict {
        let outcome = tinyflows::engine::RunOutcome {
            // Non-empty: the mechanical pre-judge must not settle this one,
            // because the point is what the MODEL's answer becomes.
            output: serde_json::json!({ "nodes": { "fetch": { "json": 1 } } }),
            pending_approvals: Vec::new(),
            cancelled: false,
        };
        let diagnosis = Diagnosis::default();
        let evidence = Evidence {
            outcome: &outcome,
            diagnosis: &diagnosis,
            changed: String::new(),
            failed,
        };
        let caps = tinyflows::caps::Capabilities {
            llm: std::sync::Arc::new(Says(blocker)),
            ..tinyflows::caps::mock::mock_capabilities()
        };
        judge(&Goal::new("do the thing"), &evidence, &caps, None)
            .await
            .expect("judged")
    }

    #[tokio::test]
    async fn a_mechanically_broken_run_cannot_be_called_terminal() {
        // Field observation: a shell step exited nonzero, the judge answered
        // `missing_evidence`, and the episode ended with two of its three
        // attempts unused — when rewriting the script was the whole fix.
        // The prompt says mechanical failures are goal_not_met; a model that
        // misreads it must not get to end the episode anyway.
        let verdict = verdict_for("missing_evidence", Some("script exited 5".into())).await;
        assert_eq!(verdict.blocker, Blocker::GoalNotMet);
        assert!(verdict.blocker.continuable());
    }

    #[tokio::test]
    async fn a_run_that_completed_keeps_the_judges_terminal_verdict() {
        // No mechanical failure: the judge is the authority on whether
        // another attempt could help, and this downgrade must not become a
        // blanket refusal to ever stand down.
        let verdict = verdict_for("missing_evidence", None).await;
        assert_eq!(verdict.blocker, Blocker::MissingEvidence);
    }

    #[tokio::test]
    async fn a_broken_run_still_waiting_on_a_person_stays_terminal() {
        // NeedsInput and ExternalWait survive the downgrade: both mean
        // something OUTSIDE the loop must move, which a broken run does not
        // change.
        let verdict = verdict_for("needs_input", Some("script exited 5".into())).await;
        assert_eq!(verdict.blocker, Blocker::NeedsInput);
    }

    use serde_json::json;
    use tinyflows::diagnostics::{HiddenError, NeverRan, NullBinding};

    fn outcome(output: serde_json::Value) -> RunOutcome {
        RunOutcome {
            output,
            pending_approvals: Vec::new(),
            cancelled: false,
        }
    }

    fn evidence<'a>(o: &'a RunOutcome, d: &'a Diagnosis) -> Evidence<'a> {
        Evidence {
            outcome: o,
            diagnosis: d,
            changed: String::new(),
            failed: None,
        }
    }

    #[test]
    fn a_parked_approval_needs_no_model() {
        let mut o = outcome(json!({}));
        o.pending_approvals = vec!["gate".into()];
        let d = Diagnosis::default();
        let verdict = without_a_model(&evidence(&o, &d)).expect("settled without a model");
        assert_eq!(verdict.blocker, Blocker::NeedsInput);
        assert!(
            verdict.advanced,
            "reaching the gate is progress, not a stall"
        );
    }

    #[test]
    fn a_cancelled_run_did_not_fail_it_was_stopped() {
        let mut o = outcome(json!({ "nodes": { "a": {} } }));
        o.cancelled = true;
        let d = Diagnosis::default();
        let verdict = without_a_model(&evidence(&o, &d)).expect("settled");
        assert_eq!(verdict.blocker, Blocker::ExternalWait);
        assert!(
            !verdict.blocker.continuable(),
            "retrying now is not retrying later"
        );
    }

    #[test]
    fn a_run_where_nothing_ran_and_nothing_changed_is_terminal() {
        let o = outcome(json!({}));
        let d = Diagnosis {
            never_ran: vec![NeverRan {
                node_id: "work".into(),
                routed_by: Some("gate".into()),
            }],
            ..Diagnosis::default()
        };
        let verdict = without_a_model(&evidence(&o, &d)).expect("settled");
        assert_eq!(verdict.blocker, Blocker::MissingEvidence);
        assert!(!verdict.blocker.continuable());
    }

    #[test]
    fn a_run_that_produced_something_goes_to_the_model() {
        let o = outcome(json!({ "nodes": { "a": { "items": [1] } } }));
        let d = Diagnosis::default();
        assert!(
            without_a_model(&evidence(&o, &d)).is_none(),
            "a real outcome is a judgement, not a fact"
        );
    }

    #[test]
    fn an_unverifiable_null_binding_is_not_reported_as_a_finding() {
        // The engine marks expressions it could not evaluate even in principle.
        // Reporting those buries the ones that are real.
        let o = outcome(json!({}));
        let d = Diagnosis {
            null_bindings: vec![NullBinding {
                node_id: "a".into(),
                location: "config.prompt".into(),
                expression: "=nodes.x.item".into(),
                unverifiable: true,
                reads_from: None,
                suggestion: "n/a".into(),
            }],
            ..Diagnosis::default()
        };
        assert!(evidence(&o, &d).findings().is_empty());
    }

    #[test]
    fn a_swallowed_error_reaches_the_judge() {
        // The failure a naive reading misses entirely: the step is marked
        // failed and its diagnostics are empty.
        let o = outcome(json!({}));
        let d = Diagnosis {
            hidden_errors: vec![HiddenError {
                node_id: "fetch".into(),
                message: Some("404".into()),
            }],
            ..Diagnosis::default()
        };
        let findings = evidence(&o, &d).findings();
        assert_eq!(findings.len(), 1);
        assert!(findings[0].contains("swallowed"), "{findings:?}");
        assert!(findings[0].contains("404"));
    }

    #[test]
    fn a_null_binding_names_the_node_it_should_have_read_from() {
        let o = outcome(json!({}));
        let d = Diagnosis {
            null_bindings: vec![NullBinding {
                node_id: "review".into(),
                location: "config.prompt".into(),
                expression: "=nodes.fetch.item.body".into(),
                unverifiable: false,
                reads_from: Some("fetch".into()),
                suggestion: "did you mean .item.json.body".into(),
            }],
            ..Diagnosis::default()
        };
        let findings = evidence(&o, &d).findings();
        assert!(findings[0].contains("reading from `fetch`"), "{findings:?}");
        assert!(
            findings[0].contains("item.json.body"),
            "the suggestion carries"
        );
    }

    #[test]
    fn a_clean_run_says_so_rather_than_showing_an_empty_list() {
        let o = outcome(json!({ "nodes": {} }));
        let d = Diagnosis::default();
        assert!(evidence(&o, &d).render().contains("found nothing wrong"));
    }
}
