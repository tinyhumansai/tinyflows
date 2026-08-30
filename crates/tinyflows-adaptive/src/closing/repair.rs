//! Fixing the graph, when the graph is what fell short.
//!
//! The other half of learning. A lesson changes what the *next plan* thinks;
//! this changes the procedure itself — an edge that was never wired, a binding
//! that read the envelope instead of its `json` field, a node routed past by a
//! condition that could not be true.
//!
//! Three rules make this safe to run unattended.
//!
//! **A repair is a variant, never an overwrite.** The parent has a score
//! ([`crate::ledger::Ledger::workflow_score`]) built from every run it has ever
//! had. Editing it in place destroys that evidence and leaves nothing to
//! compare the fix against — a "learning" system that cannot tell whether it
//! learned. The variant starts at 0/0 and has to earn its way past the parent.
//!
//! **Only when the graph is actually at fault.** An agent that ran, was wired
//! correctly, and simply did a poor job is not a graph problem, and rewriting
//! the graph in response churns the store while fixing nothing. The gate is
//! mechanical and runs before any inference.
//!
//! **No renames.** [`GraphOp::RenameNode`] rewires edges but does not rewrite
//! `=nodes.<old_id>…` expressions inside other nodes' configs — the graph
//! validates and then runs quietly wrong. A person doing this by hand can
//! re-point the bindings; a batch arriving from a model cannot be trusted to,
//! so the op is refused here.

use std::sync::Arc;

use tinyflows::caps::Capabilities;
use tinyflows::graph_ops::{GraphOp, apply_ops};
use tinyflows::store::{WorkflowRecord, WorkflowStore};
use tinyflows::validate::validate_all;

use super::judge::Evidence;
use crate::contracts::{Goal, Tier, Verdict};
use crate::intake::{IntakeError, Result, ask};
use crate::ledger::Ledger;

const SYSTEM: &str = "\
You repair a workflow graph that ran and fell short.

You are given the graph, the engine's own diagnosis of what its steps did, and
the judge's account of what is still missing. Return the smallest batch of edits
that would fix it.

Return JSON: {\"ops\": [...], \"why\": str}

Each op is one object, tagged by `op`:
  {\"op\": \"add_node\", \"node\": {...}}
  {\"op\": \"update_node_config\", \"id\": str, \"config\": {...}}
      A JSON merge patch: keys merge onto the existing config, a null deletes.
      This is the op for fixing a wrong binding, and usually the only one needed.
  {\"op\": \"set_node_name\", \"id\": str, \"name\": str}
  {\"op\": \"remove_node\", \"id\": str}
  {\"op\": \"add_edge\", \"edge\": {\"from_node\": str, \"to_node\": str}}
  {\"op\": \"remove_edge\", \"from_node\": str, \"to_node\": str}
  {\"op\": \"set_workflow_inputs\", \"inputs\": [...]}

Do not rename nodes. A rename rewires edges but leaves every `=nodes.<old_id>`
expression pointing at a node that no longer exists, and the graph will validate
and then run wrong.

Return an empty ops list when the graph is not the problem. A workflow whose
steps were wired correctly and whose agent simply did poor work does not get
better by being edited, and an edit made anyway costs the next run its
procedure.

Prefer one precise edit to several speculative ones. You will see the result of
this batch before anything else changes.";

/// A repaired copy of a workflow that fell short.
#[derive(Debug, Clone)]
pub struct Variant {
    /// The saved record. Its id is derived from the parent and the edits, so an
    /// identical repair proposed twice lands on one variant rather than two.
    pub record: WorkflowRecord,
    /// The workflow this was derived from, whose score it must beat.
    pub parent_id: String,
    /// The edits that produced it.
    pub ops: Vec<GraphOp>,
    /// Why, in the model's words. Carried into `Approach::Variant`.
    pub why: String,
}

/// Is this a shortfall a graph edit could plausibly fix?
///
/// Runs before inference, because the common case — an agent ran, was wired
/// right, and fell short on the work — must not pay for a repair proposal it
/// will discard. A null binding, an empty prompt, a swallowed error or a node
/// that never ran are all structural; so is a judge that named a node.
#[must_use]
pub fn graph_is_suspect(verdict: &Verdict, evidence: &Evidence<'_>) -> bool {
    let d = evidence.diagnosis;
    d.null_bindings.iter().any(|b| !b.unverifiable)
        || !d.empty_prompts.is_empty()
        || !d.hidden_errors.is_empty()
        || !d.never_ran.is_empty()
        || !verdict.attributed_to.trim().is_empty()
}

/// Propose a graph fix and save it as a variant of `parent_id`.
///
/// Returns `Ok(None)` when nothing is worth changing — the graph is not
/// suspect, or the model declined. That is the expected answer most of the time
/// and is not a failure.
///
/// # Errors
/// When the store cannot be read or written, inference fails, or the proposed
/// batch does not apply, does not validate, or names something this host does
/// not have. A refused batch is an error rather than a silent `None` because
/// the caller records it: a repair that keeps failing the same gate is itself
/// evidence about the goal.
#[allow(clippy::too_many_arguments)]
pub async fn repair(
    goal: &Goal,
    verdict: &Verdict,
    evidence: &Evidence<'_>,
    parent_id: &str,
    store: &Arc<dyn WorkflowStore>,
    ledger: &dyn Ledger,
    caps: &Capabilities,
    conn: Option<&str>,
) -> Result<Option<Variant>> {
    if !graph_is_suspect(verdict, evidence) {
        return Ok(None);
    }
    let parent = store
        .get(parent_id)
        .map_err(|e| IntakeError::Store(e.to_string()))?
        .ok_or_else(|| IntakeError::Store(format!("no workflow '{parent_id}'")))?;

    let user = format!(
        "# Goal\n{}\n\n# The workflow that ran\n{}\n\n# What is still missing\n{}{}\n\n{}",
        goal.text.trim(),
        serde_json::to_string_pretty(&parent.graph)
            .map_err(|e| IntakeError::Store(e.to_string()))?,
        verdict.gap,
        if verdict.attributed_to.trim().is_empty() {
            String::new()
        } else {
            format!(
                "\n\nThe judge attributed this to node `{}`.",
                verdict.attributed_to
            )
        },
        evidence.render()
    );

    let answer = ask(caps, conn, Tier::Repair, SYSTEM, &user).await?;
    let ops = read_ops(&answer)?;
    if ops.is_empty() {
        return Ok(None);
    }

    // Applied to a copy and validated before anything is saved — the same order
    // the engine's own authoring path uses, and for the same reason: a store
    // whose listings are trustworthy is one nothing unrunnable can enter.
    let graph = apply_ops(&parent.graph, &ops)
        .map_err(|e| IntakeError::Invalid(format!("the repair does not apply: {e}")))?;
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

    let id = variant_id(parent_id, &ops);
    let why = answer["why"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .to_string();
    let record = WorkflowRecord {
        id: id.clone(),
        name: format!("{} (repaired)", parent.name),
        description: if why.is_empty() {
            format!("Variant of {parent_id}.")
        } else {
            format!("Variant of {parent_id}: {why}")
        },
        enabled: parent.enabled,
        defaults: parent.defaults.clone(),
        graph,
        // Never inherited: it points at the parent's file, and saving under it
        // would overwrite the very record this exists to leave intact.
        source_path: None,
    };
    store
        .policy()
        .check_graph(&id, &record.graph)
        .map_err(|e| IntakeError::Unsupported(e.to_string()))?;
    store
        .save(&record)
        .map_err(|e| IntakeError::Store(e.to_string()))?;

    // Recorded after the save, so a link never points at a graph the store
    // refused. Without it the variant is just another row in the catalogue and
    // the promotion gate has no family to compare within.
    //
    // The converse can happen under a buffering store, and is fine: this link
    // is durable now, while the graph lands only when the host flushes — and a
    // host may gate that flush on the episode succeeding, so a failed episode
    // leaves a link with no graph behind it. That degrades to "not offerable"
    // in the catalogue rather than breaking anything, and the same failure
    // re-derives the same repair onto the same content-derived id later, so
    // the lineage and score recorded now reattach instead of being orphaned.
    ledger.link_variant(parent_id, &id).await?;

    Ok(Some(Variant {
        record,
        parent_id: parent_id.to_string(),
        ops,
        why,
    }))
}

/// Read the batch, refusing renames.
fn read_ops(answer: &serde_json::Value) -> Result<Vec<GraphOp>> {
    let Some(raw) = answer.get("ops") else {
        return Ok(Vec::new());
    };
    if raw.is_null() {
        return Ok(Vec::new());
    }
    let ops: Vec<GraphOp> = serde_json::from_value(raw.clone())
        .map_err(|e| IntakeError::Invalid(format!("not a batch of graph ops: {e}")))?;
    if ops
        .iter()
        .any(|op| matches!(op, GraphOp::RenameNode { .. }))
    {
        return Err(IntakeError::Invalid(
            "a repair may not rename a node: edges are rewired but `=nodes.<id>` \
             expressions in other nodes are not, and the graph would validate and \
             run wrong"
                .to_string(),
        ));
    }
    Ok(ops)
}

/// `<parent>-fix-<hash of the edits>`.
///
/// Derived rather than counted so it needs no clock and no read of what already
/// exists, and so the same repair proposed twice converges on one variant
/// instead of filling the store with near-identical copies.
fn variant_id(parent_id: &str, ops: &[GraphOp]) -> String {
    // The stable digest — this id keys workflow records, scores and lineage,
    // so it must survive toolchain upgrades. See `reuse::digest_hex`.
    format!(
        "{parent_id}-fix-{}",
        crate::reuse::digest_hex(&serde_json::to_vec(ops).unwrap_or_default())
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::Blocker;
    use tinyflows::diagnostics::{Diagnosis, NeverRan, NullBinding};
    use tinyflows::engine::RunOutcome;

    fn verdict(attributed_to: &str) -> Verdict {
        Verdict {
            satisfied: false,
            blocker: Blocker::GoalNotMet,
            gap: "the report was never written".into(),
            attributed_to: attributed_to.into(),
            evidence: String::new(),
            advanced: true,
        }
    }

    fn outcome() -> RunOutcome {
        RunOutcome {
            output: serde_json::json!({}),
            pending_approvals: Vec::new(),
            cancelled: false,
        }
    }

    #[test]
    fn a_clean_run_that_simply_fell_short_is_not_a_graph_problem() {
        let d = Diagnosis::default();
        let out = outcome();
        let evidence = Evidence {
            outcome: &out,
            diagnosis: &d,
            changed: "wrote report.md".into(),
            failed: None,
        };
        assert!(!graph_is_suspect(&verdict(""), &evidence));
    }

    #[test]
    fn a_node_the_judge_named_makes_the_graph_suspect() {
        let d = Diagnosis::default();
        let out = outcome();
        let evidence = Evidence {
            outcome: &out,
            diagnosis: &d,
            changed: String::new(),
            failed: None,
        };
        assert!(graph_is_suspect(&verdict("summarise"), &evidence));
    }

    #[test]
    fn a_node_that_never_ran_makes_the_graph_suspect() {
        let d = Diagnosis {
            never_ran: vec![NeverRan {
                node_id: "publish".into(),
                routed_by: None,
            }],
            ..Diagnosis::default()
        };
        let out = outcome();
        let evidence = Evidence {
            outcome: &out,
            diagnosis: &d,
            changed: String::new(),
            failed: None,
        };
        assert!(graph_is_suspect(&verdict(""), &evidence));
    }

    #[test]
    fn an_unverifiable_null_binding_alone_does_not_make_it_suspect() {
        // The engine could not evaluate the expression even in principle, so it
        // is not evidence the graph is wrong — and repairing on it would edit a
        // correct graph every run.
        let d = Diagnosis {
            null_bindings: vec![NullBinding {
                node_id: "fetch".into(),
                location: "config.prompt".into(),
                expression: "=nodes.agent.item.body".into(),
                unverifiable: true,
                reads_from: Some("agent".into()),
                suggestion: "run it for real".into(),
            }],
            ..Diagnosis::default()
        };
        let out = outcome();
        let evidence = Evidence {
            outcome: &out,
            diagnosis: &d,
            changed: String::new(),
            failed: None,
        };
        assert!(!graph_is_suspect(&verdict(""), &evidence));
    }

    #[test]
    fn declining_to_edit_is_read_as_no_ops_not_as_a_malformed_reply() {
        assert!(
            read_ops(&serde_json::json!({"ops": [], "why": "the graph is fine"}))
                .expect("no ops")
                .is_empty()
        );
        assert!(
            read_ops(&serde_json::json!({"why": "nothing to do"}))
                .expect("no ops")
                .is_empty()
        );
        assert!(
            read_ops(&serde_json::json!({"ops": null}))
                .expect("no ops")
                .is_empty()
        );
    }

    #[test]
    fn a_rename_is_refused_even_though_the_engine_would_apply_it() {
        let batch = serde_json::json!({
            "ops": [{"op": "rename_node", "id": "a", "new_id": "b"}]
        });
        let err = read_ops(&batch).expect_err("refused");
        assert!(err.to_string().contains("rename"), "{err}");
    }

    #[test]
    fn an_ordinary_config_patch_reads_as_one_op() {
        let batch = serde_json::json!({
            "ops": [{
                "op": "update_node_config",
                "id": "summarise",
                "config": {"prompt": "=nodes.fetch.item.json.body"}
            }]
        });
        let ops = read_ops(&batch).expect("read");
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].name(), "update_node_config");
    }

    #[test]
    fn the_same_repair_twice_lands_on_the_same_variant_id() {
        let ops = vec![GraphOp::SetNodeName {
            id: "a".into(),
            name: "A".into(),
        }];
        assert_eq!(variant_id("weekly", &ops), variant_id("weekly", &ops));
        let other = vec![GraphOp::SetNodeName {
            id: "a".into(),
            name: "B".into(),
        }];
        assert_ne!(variant_id("weekly", &ops), variant_id("weekly", &other));
    }

    #[test]
    fn a_variant_id_names_its_parent() {
        let ops = vec![GraphOp::RemoveNode { id: "a".into() }];
        assert!(variant_id("weekly-report", &ops).starts_with("weekly-report-fix-"));
    }
}
