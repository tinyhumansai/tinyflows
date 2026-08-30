//! Building an [`ApprovalRequest`] from a node's config, and reading a
//! settled [`ApprovalDecision`] back out of a resume value, the run's
//! approvals list, or a finished review.
//!
//! Split out of `approval.rs` to keep that file under the repository's
//! line-length limit; request-building and decision-reading are one cohesive
//! concern (every function here is pure data shaping, none of it waits or
//! calls a capability), so they belong together rather than split further.

use serde_json::{Value, json};

use crate::caps::{ApprovalDecision, ApprovalRequest, ApprovalSubject};
use crate::data::Item;
use crate::error::{EngineError, Result};
use crate::nodes::NodeContext;

/// The default rendering hint when the graph does not say what the subject is.
const DEFAULT_SUBJECT_KIND: &str = "json";

/// The run's host id, read **only** from the run-level slots a host seeds
/// (`run.id`, `run.run_id`) — never from the trigger payload.
///
/// Whichever of these is populated becomes part of `request_id`, the provider's
/// create-or-fetch key, so this lookup is a trust boundary rather than a
/// convenience. `run.trigger.*` is the payload a caller hands to
/// `engine::run`; for a webhook or any user-facing trigger that payload is
/// attacker-influenced, and reading a run id out of it would let an attacker
/// pick a `request_id` colliding with an earlier run and inherit its cached
/// decision — approving a new, unreviewed subject without a human ever seeing
/// it. `run.id` / `run.run_id` sit outside the trigger, so a host puts a
/// server-generated value there deliberately.
///
/// A host must still seed a **server-generated** id (or set
/// `config.request_id` from one) and never copy a caller-supplied field into
/// these slots: the crate is host-agnostic and cannot tell trusted run
/// metadata from untrusted, so enforcing that is the host's job, as it is for
/// any identity used as a de-duplication or idempotency key.
fn run_id(ctx: &NodeContext<'_>) -> Option<String> {
    ["id", "run_id"]
        .iter()
        .find_map(|key| ctx.run.get(*key).and_then(Value::as_str))
        .map(str::to_string)
}

/// Builds the review request from the node's resolved config.
///
/// The `request_id` is what makes the provider's create-or-fetch contract
/// work, so it must be **stable across activations**: an interrupt discards the
/// activation's state update, so the node cannot remember an id it generated,
/// and anything derived from the clock or a counter would create a fresh review
/// on every resume. Hence run id + node id, or an explicit `config.request_id`
/// for a host that wants to key reviews its own way.
///
/// Falling back to the bare node id when *neither* is available would let two
/// different runs of the same graph collide on the same `request_id`: since
/// [`ApprovalProvider::decide`](crate::caps::ApprovalProvider::decide) is
/// create-or-fetch, a later run would silently inherit an earlier run's
/// decision and route an unreviewed subject straight through `approved`. So a
/// node with no `config.request_id` and no run-scoped identity is a
/// configuration error, not a degraded default.
pub(super) fn build_request(ctx: &NodeContext<'_>, config: &Value) -> Result<ApprovalRequest> {
    let run = run_id(ctx);
    let request_id = match config.get("request_id").and_then(Value::as_str) {
        Some(explicit) => explicit.to_string(),
        None => match &run {
            Some(run) => format!("{run}:{}", ctx.node.id),
            None => {
                return Err(EngineError::Capability(format!(
                    "approval node {:?}: no `request_id` configured and no run-scoped identity \
                     available (expected `run.id` or `run.run_id`, which a host seeds outside the \
                     caller-supplied trigger payload); set \
                     `config.request_id` explicitly or seed a run id, otherwise later runs could \
                     reuse an earlier run's decision",
                    ctx.node.id
                )));
            }
        },
    };

    // The subject defaults to the item that arrived, which is the common case:
    // a node upstream produced the thing, and the human looks at it.
    let value = config
        .get("subject")
        .cloned()
        .or_else(|| ctx.input.first().map(|item| item.json.clone()))
        .unwrap_or(Value::Null);

    // `validate::validate_all` only sees `assignees` as authored: a literal
    // non-array (a bare string, the natural single-reviewer mistake) or a
    // literal empty array are both refused there. An `=`-bound `assignees`
    // (e.g. `"=item.reviewers"`) is a string at author time — it passes that
    // check by looking like *some* other field entirely — and resolves to
    // its real shape only here, at execution time. So the same two refusals
    // apply again to the resolved value: present and not an array, or
    // present, an array, and empty (or empty of strings) once resolved. Both
    // reach the same nobody-reviews-this audience a validated graph should
    // never produce.
    let assignees = match config.get("assignees") {
        None => Vec::new(),
        Some(value) => match value.as_array() {
            None => {
                return Err(EngineError::Capability(format!(
                    "approval node {:?}: `assignees` resolved to {value}, not an array of strings",
                    ctx.node.id
                )));
            }
            Some(values) => {
                // Every element must be a string. Dropping the ones that are
                // not would route the review to a *different* audience than the
                // graph asked for, silently: `["=item.reviewer", 42]` would
                // quietly become a one-reviewer list. Losing a reviewer is
                // exactly the kind of quiet change a review must not make.
                let mut assignees: Vec<String> = Vec::with_capacity(values.len());
                for value in values {
                    let Some(handle) = value.as_str() else {
                        return Err(EngineError::Capability(format!(
                            "approval node {:?}: `assignees` entry {value} is not a string; a \
                             reviewer handle that resolved to something else would be dropped \
                             and the review routed to a smaller audience than authored",
                            ctx.node.id
                        )));
                    };
                    assignees.push(handle.to_string());
                }
                if assignees.is_empty() {
                    return Err(EngineError::Capability(format!(
                        "approval node {:?}: `assignees` resolved to an empty array; a review \
                         with nobody assigned can never be resolved",
                        ctx.node.id
                    )));
                }
                assignees
            }
        },
    };

    Ok(ApprovalRequest {
        request_id,
        node_id: ctx.node.id.clone(),
        run_id: run,
        title: string_field(config, "title"),
        prompt: string_field(config, "prompt"),
        subject: ApprovalSubject {
            kind: config
                .get("subject_kind")
                .and_then(Value::as_str)
                .unwrap_or(DEFAULT_SUBJECT_KIND)
                .to_string(),
            value,
        },
        assignees,
        metadata: config.get("metadata").cloned().unwrap_or(Value::Null),
    })
}

/// A config field read as a string, ignoring a non-string (an unresolved
/// expression that came back `null`, say).
fn string_field(config: &Value, key: &str) -> Option<String> {
    config.get(key).and_then(Value::as_str).map(str::to_string)
}

/// Whether `list` (an array of strings) names this node or its review.
pub(super) fn names(list: Option<&Value>, request: &ApprovalRequest) -> bool {
    list.and_then(Value::as_array).is_some_and(|ids| {
        ids.iter()
            .filter_map(Value::as_str)
            .any(|id| id == request.node_id || id == request.request_id)
    })
}

/// Reads a decision out of a resume value, if it carries one.
///
/// Accepts the engine's own `{"rejected": [<id>…]}` denial shape (checked
/// first, so a denial always beats an approval delivered in the same value),
/// the mirror `{"approved": [<id>…]}`, and a full verdict object — either
/// inline or nested under `decision`.
///
/// The array forms above are always scoped to this request via [`names`], the
/// same way [`super::gate`](crate::nodes::integration::gate) scopes its own
/// `approved` array — required, because several nodes can be interrupted at
/// once and a resume value is not addressed to just one of them. The inline
/// verdict-object form carries no such array to check, so it is accepted
/// unscoped **only** when it does not itself name a different request —
/// matching the same "single-interrupt convenience" precedent
/// `engine::build::activation`'s bare `Value::Bool(true)` case documents, but
/// without silently absorbing a verdict a host explicitly addressed elsewhere.
pub(super) fn decision_from_resume(
    resume: &Value,
    request: &ApprovalRequest,
) -> Option<ApprovalDecision> {
    if names(resume.get("rejected"), request) {
        return Some(ApprovalDecision::rejected(
            resume
                .get("comment")
                .and_then(Value::as_str)
                .map(str::to_string),
        ));
    }
    if names(resume.get("approved"), request) {
        return Some(ApprovalDecision::approved());
    }

    // A verdict object must say WHICH review it settles. One resume value is
    // delivered to every interrupted node, so an unaddressed `{"approved":
    // true}` would settle whichever reviews happen to read it — approving every
    // pending review at once, without the sender needing to know a single id.
    // An unaddressed verdict is therefore ignored rather than assumed to be
    // ours; the array forms carry their ids and stay supported.
    let verdict = resume.get("decision").unwrap_or(resume);
    let named = verdict
        .get("node_id")
        .or_else(|| verdict.get("request_id"))
        .and_then(Value::as_str)?;
    if named != request.node_id && named != request.request_id {
        // Addressed to a different node's review; not ours to take.
        return None;
    }
    let approved = verdict.get("approved").and_then(Value::as_bool)?;
    Some(ApprovalDecision {
        approved,
        decided_by: string_field(verdict, "decided_by"),
        comment: string_field(verdict, "comment"),
        payload: verdict.get("payload").cloned(),
    })
}

/// The decision already in hand before the host is asked: a resume value, or
/// this node's id on the run's approvals list.
pub(super) fn delivered(
    ctx: &NodeContext<'_>,
    request: &ApprovalRequest,
) -> Option<ApprovalDecision> {
    if let Some(decision) = ctx
        .resume
        .as_ref()
        .and_then(|resume| decision_from_resume(resume, request))
    {
        return Some(decision);
    }

    // The re-execute resume path: `engine::resume` merges newly-approved ids
    // into the run input, and they arrive here as the top-level `run.approvals`
    // — the **explicit** channel (`RunInput::approvals`), which a host fills
    // deliberately.
    //
    // `run.trigger.approvals` is deliberately NOT read, even though
    // `engine::resume` also writes the merged list there. The trigger is the
    // payload a caller hands to `engine::run`, so honouring it would let anyone
    // who can start a run post `{"approvals": ["<node id>"]}` and approve their
    // own review on the initial execution — skipping the human entirely. A
    // review that can be self-approved by its own subject is not a review.
    //
    // Known residual, and why it is not fixed here: `merge_approvals` seeds its
    // starting set from `trigger["approvals"]`, so a trigger-supplied id is
    // folded into the explicit list *on a resume*. That is pre-existing engine
    // behaviour shared with the `requires_approval` gate in
    // `engine::build::activation`, and narrowing it changes resume semantics for
    // every gate, not just this node — so it belongs in its own change rather
    // than riding along here. The initial-run bypass, which is the reachable-
    // without-a-host-action one, is closed.
    if names(ctx.run.get("approvals"), request) {
        return Some(ApprovalDecision::approved());
    }
    None
}

/// The item a settled review emits.
///
/// `subject` is what the human actually signed off on — their edit when the
/// host's surface allowed one, otherwise exactly what was sent — so a
/// downstream node reads one field regardless. The original input is kept under
/// `input` so nothing is lost when the subject was a projection of it.
pub(super) fn decided_item(
    request: &ApprovalRequest,
    decision: &ApprovalDecision,
    input: Value,
) -> Item {
    Item::new(json!({
        "approved": decision.approved,
        "subject": decision
            .payload
            .clone()
            .unwrap_or_else(|| request.subject.value.clone()),
        "subject_kind": request.subject.kind,
        "edited": decision.payload.is_some(),
        "decided_by": decision.decided_by,
        "comment": decision.comment,
        "request_id": request.request_id,
        "input": input,
    }))
}

/// The slot state a settled review records, so `=nodes.<id>.decision.approved`
/// resolves from anywhere in the graph — including from a branch that did not
/// receive the emitted item (a `drop`ped rejection has no item at all).
pub(super) fn decision_meta(decision: &ApprovalDecision, request: &ApprovalRequest) -> Value {
    json!({
        "decision": {
            "approved": decision.approved,
            "decided_by": decision.decided_by,
            "comment": decision.comment,
            "request_id": request.request_id,
        }
    })
}
