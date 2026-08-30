//! The `approval` node: put something in front of a human, route on their answer.
//!
//! A workflow that posts, pays, emails, or deletes wants a person in the loop at
//! exactly one point, holding exactly one thing — a URL to look at, a draft to
//! read, a payload to sign off. That is what this node is: it carries the
//! **subject** of the review, hands it to the host's review surface through the
//! [`ApprovalProvider`](crate::caps::ApprovalProvider) capability, and emits the
//! verdict on its `approved` / `rejected` ports as ordinary data the graph can
//! branch on.
//!
//! # Not the same thing as `requires_approval`
//!
//! The `requires_approval` flag gates a node that would otherwise run: it says
//! "don't execute this until someone says go", carries nothing, and its answer
//! is a yes/no the graph cannot inspect. This node is the review *itself* —
//! addressable, with a subject, a reviewer, a comment, and a rejection branch.
//! Use the flag to hold back a dangerous node; use this kind when the decision
//! is a step in the workflow.
//!
//! # How waiting works
//!
//! Same two shapes as [`gate`](super::gate), and for the same reasons. A human
//! review is measured in minutes-to-days, so `wait_mode: "suspend"` (the
//! **default** here, unlike `gate`) interrupts the run: nothing is burned while
//! the card sits in someone's queue, and the host resumes the run when they
//! answer. `wait_mode: "poll"` re-activates the node on an interval instead,
//! which is right only when the decision is expected within seconds — each poll
//! costs a super-step and a node visit against the run's budgets, so the poll
//! count is bounded here rather than left to the run-level backstop.
//!
//! # Where a decision can come from
//!
//! In priority order, because more than one channel can be live at once:
//!
//! 1. **The resume value** ([`NodeContext::resume`]) — the checkpointed-resume
//!    path, which replays from the checkpoint rather than re-running with a
//!    merged run input, so it is the *only* channel there. A rejection in the
//!    engine's `{"rejected": [<node id>]}` shape wins over everything, matching
//!    how a `requires_approval` gate treats a denial.
//! 2. **The run's approvals list** (`run.trigger.approvals`) — the
//!    re-execute resume path, and how `engine::resume` has always delivered an
//!    approval. Listing this node's id there approves it.
//! 3. **The host's [`ApprovalProvider`]** — asked once per activation, under
//!    the create-or-fetch contract documented on the trait.
//! 4. **Nobody**, on a host that wired no provider: the node simply waits, so
//!    it reduces to a pause the host settles through `engine::resume`.

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::caps::{ApprovalOutcome, ApprovalRequest};
use crate::data::Item;
use crate::error::{EngineError, Result};
use crate::nodes::{NodeContext, NodeExecutor, NodeOutput, resolve_config_traced};

#[path = "approval_request.rs"]
mod approval_request;
use approval_request::{build_request, decided_item, decision_meta, delivered};

/// Default gap between polls, in milliseconds. A second, not the `gate`'s 250ms:
/// nothing a human does resolves faster than that.
const DEFAULT_POLL_INTERVAL_MS: u64 = 1_000;

/// Default ceiling on polls before the wait budget is called spent. With the
/// default interval that is a minute of waiting — deliberately short, because
/// polling is the wrong mode for a long review and the timeout should say so
/// rather than quietly spending a run's whole visit budget.
const DEFAULT_MAX_POLLS: u64 = 60;

/// The slot key the poll count is recorded under, so it survives a checkpoint
/// the way a `loop` node's iteration does.
const POLLS_KEY: &str = "polls";

/// How the node waits for a human.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitMode {
    /// Interrupt the run; the host resumes it when the answer lands. The
    /// default — a review is a human-timescale wait.
    Suspend,
    /// Re-activate on an interval and ask again. Only sane for a decision
    /// expected within seconds.
    Poll,
}

impl WaitMode {
    fn from_config(config: &Value) -> Self {
        match config.get("wait_mode").and_then(Value::as_str) {
            Some("poll") => Self::Poll,
            _ => Self::Suspend,
        }
    }
}

/// What a rejection does to the graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OnReject {
    /// Emit the verdict on the `rejected` port (default), so a graph can run a
    /// recovery branch — notify the author, revise, ask again.
    Route,
    /// Fail the node, letting the ordinary `on_error` policy take over.
    Error,
    /// Emit nothing and let the branch end here.
    Drop,
}

impl OnReject {
    fn from_config(config: &Value) -> Self {
        match config.get("on_reject").and_then(Value::as_str) {
            Some("error") => Self::Error,
            Some("drop") => Self::Drop,
            _ => Self::Route,
        }
    }
}

/// What a spent poll budget does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OnTimeout {
    /// Fail the node (default): nobody answered, and pretending otherwise is
    /// how an unreviewed payload ships.
    Error,
    /// Treat silence as a rejection and follow the `on_reject` policy.
    Reject,
    /// Emit on the `timeout` port, so escalation is its own branch.
    Route,
}

impl OnTimeout {
    fn from_config(config: &Value) -> Self {
        match config.get("on_timeout").and_then(Value::as_str) {
            Some("reject") => Self::Reject,
            Some("route") => Self::Route,
            _ => Self::Error,
        }
    }
}

/// Presents a subject to a human and routes on approve / reject.
#[derive(Debug, Default, Clone)]
pub struct ApprovalNode;

/// Reads a positive integer config field, falling back to `default`.
fn positive_u64(config: &Value, key: &str, default: u64) -> u64 {
    config
        .get(key)
        .and_then(Value::as_u64)
        .filter(|n| *n > 0)
        .unwrap_or(default)
}

#[async_trait]
impl NodeExecutor for ApprovalNode {
    async fn execute(&self, ctx: NodeContext<'_>) -> Result<NodeOutput> {
        let (config, diagnostics) = resolve_config_traced(&ctx);
        let request = build_request(&ctx, &config)?;
        let input = ctx
            .input
            .first()
            .map(|item| item.json.clone())
            .unwrap_or(Value::Null);

        // Ask the host only when nothing has already settled this review: a
        // resume or a listed approval is the answer, and re-asking would put a
        // decided review back in front of the provider.
        let outcome = match delivered(&ctx, &request) {
            Some(decision) => {
                // A provider may have a card open for this review from an
                // earlier activation's `decide` call (it went `Pending`, or
                // this run never asked because the answer already arrived some
                // other way). Either way nobody is waiting on the provider's
                // card any more, so withdraw it rather than leave a stale entry
                // in the host's queue. Best-effort: the run has already decided
                // what to do, and a failed withdrawal must not change that.
                if let Some(provider) = ctx.caps.approvals.as_ref() {
                    if let Err(err) = provider
                        .cancel(&request.request_id, "resolved via resume")
                        .await
                    {
                        tracing::warn!(
                            node = %ctx.node.id,
                            request = %request.request_id,
                            error = %err,
                            "withdrawing the provider's review after a resume decision failed"
                        );
                    }
                }
                ApprovalOutcome::Decided(decision)
            }
            None => match ctx.caps.approvals.as_ref() {
                Some(provider) => provider.decide(&request).await?,
                // No provider: the node is a pause the host settles out of band
                // through `engine::resume`, which is exactly what waiting does.
                None => ApprovalOutcome::Pending,
            },
        };

        let decision = match outcome {
            ApprovalOutcome::Decided(decision) => decision,
            ApprovalOutcome::Pending => return self.wait(&ctx, &config, &request).await,
        };

        tracing::info!(
            node = %ctx.node.id,
            request = %request.request_id,
            approved = decision.approved,
            "approval decided"
        );

        let meta = decision_meta(&decision, &request);
        if decision.approved {
            return Ok(NodeOutput::routed(
                vec![decided_item(&request, &decision, input)],
                "approved",
            )
            .with_meta(meta)
            .with_diagnostics(diagnostics));
        }

        Ok(match OnReject::from_config(&config) {
            OnReject::Route => {
                NodeOutput::routed(vec![decided_item(&request, &decision, input)], "rejected")
                    .with_meta(meta)
                    .with_diagnostics(diagnostics)
            }
            OnReject::Drop => NodeOutput::empty()
                .with_meta(meta)
                .with_diagnostics(diagnostics),
            OnReject::Error => {
                return Err(EngineError::Capability(format!(
                    "approval node {:?}: rejected by {}{}",
                    ctx.node.id,
                    decision.decided_by.as_deref().unwrap_or("a reviewer"),
                    decision
                        .comment
                        .as_deref()
                        .map(|c| format!(" ({c})"))
                        .unwrap_or_default(),
                )));
            }
        })
    }
}

impl ApprovalNode {
    /// Nobody has decided yet: suspend the run, or spend a poll.
    async fn wait(
        &self,
        ctx: &NodeContext<'_>,
        config: &Value,
        request: &ApprovalRequest,
    ) -> Result<NodeOutput> {
        if WaitMode::from_config(config) == WaitMode::Suspend {
            // Suspending discards this activation's update, so no poll is
            // charged — a suspended review is not spending a budget, it is
            // waiting for a person. The payload is what the host renders or
            // routes if it did not get the request through a provider.
            return Ok(NodeOutput::interrupt(
                ctx.node.id.clone(),
                json!({
                    "kind": "approval",
                    "node": ctx.node.id,
                    "request_id": request.request_id,
                    "title": request.title,
                    "prompt": request.prompt,
                    "subject": request.subject.value,
                    "subject_kind": request.subject.kind,
                    "assignees": request.assignees,
                    "metadata": request.metadata,
                }),
            ));
        }

        let polls = ctx
            .nodes
            .get(&ctx.node.id)
            .and_then(|slot| slot.get(POLLS_KEY))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let max_polls = positive_u64(config, "max_polls", DEFAULT_MAX_POLLS);
        let meta = json!({ POLLS_KEY: polls + 1, "request_id": request.request_id });

        // `polls` counts activations that have already asked the provider
        // once (this one included), so re-entering when `polls + 1 <
        // max_polls` — rather than `polls < max_polls` — is what makes
        // `max_polls` the number of `decide` calls actually charged, not
        // `max_polls + 1`: this activation's call is the `polls + 1`-th.
        if polls + 1 < max_polls {
            let interval = positive_u64(config, "poll_interval_ms", DEFAULT_POLL_INTERVAL_MS);
            return Ok(NodeOutput::reenter_after(interval, meta));
        }

        // Budget spent. Whatever happens next, nobody is waiting on this review
        // any more, so withdraw it rather than leaving a dead card in a queue.
        // Best-effort: the run has already decided what to do, and a failed
        // withdrawal must not change that.
        if let Some(provider) = ctx.caps.approvals.as_ref() {
            if let Err(err) = provider
                .cancel(&request.request_id, "approval node timed out")
                .await
            {
                tracing::warn!(
                    node = %ctx.node.id,
                    request = %request.request_id,
                    error = %err,
                    "withdrawing the timed-out review failed"
                );
            }
        }

        // The `=nodes.<id>.decision.approved` contract other settled reviews
        // give the graph applies here too: a downstream guard reading it after
        // a timeout must see `false`, not an absent value, and a `rejected`
        // recovery branch reading `=item.comment` / `=item.input` must not get
        // `null` back just because the review timed out rather than being
        // actively declined.
        let comment = format!("no decision after {max_polls} polls");
        let timed_out = json!({
            "approved": false,
            "timed_out": true,
            "request_id": request.request_id,
            "subject": request.subject.value,
            "subject_kind": request.subject.kind,
            "edited": false,
            "decided_by": Value::Null,
            "comment": comment,
            "input": ctx.input.first().map(|item| item.json.clone()).unwrap_or(Value::Null),
        });
        let meta = json!({
            POLLS_KEY: polls + 1,
            "request_id": request.request_id,
            "decision": {
                "approved": false,
                "timed_out": true,
                "decided_by": Value::Null,
                "comment": comment,
                "request_id": request.request_id,
            }
        });

        match OnTimeout::from_config(config) {
            OnTimeout::Error => Err(EngineError::Capability(format!(
                "approval node {:?}: no decision after {max_polls} polls; raise \
                 `max_polls`/`poll_interval_ms`, switch to `wait_mode: \"suspend\"`, or wire a \
                 `timeout` port",
                ctx.node.id
            ))),
            OnTimeout::Route => {
                Ok(NodeOutput::routed(vec![Item::new(timed_out)], "timeout").with_meta(meta))
            }
            OnTimeout::Reject => match OnReject::from_config(config) {
                OnReject::Route => {
                    Ok(NodeOutput::routed(vec![Item::new(timed_out)], "rejected").with_meta(meta))
                }
                OnReject::Drop => Ok(NodeOutput::empty().with_meta(meta)),
                OnReject::Error => Err(EngineError::Capability(format!(
                    "approval node {:?}: no decision after {max_polls} polls, and \
                     `on_timeout: \"reject\"` with `on_reject: \"error\"` fails the node",
                    ctx.node.id
                ))),
            },
        }
    }
}

#[cfg(test)]
#[path = "approval_tests.rs"]
mod tests;
