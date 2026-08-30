//! Human-in-the-loop review: the [`ApprovalProvider`] capability an `approval`
//! node reaches the human through.
//!
//! # Why this is a capability at all
//!
//! An `approval` node presents *something* — a URL, a block of text, a
//! generated payload — to a person and waits for an approve/reject. Everything
//! about how that reaches a human is the host's: a Slack card, an inbox row, a
//! web review queue, a phone notification. The crate stays out of it and asks
//! only two things of the host, both expressed here: **register/fetch** a
//! review request, and (optionally) **cancel** one it will never wait on again.
//!
//! # The idempotency contract (load-bearing)
//!
//! [`ApprovalProvider::decide`] is a **create-or-fetch** call keyed on
//! [`ApprovalRequest::request_id`]: the first call with a given id creates the
//! review, every later call with that same id returns the state of *that*
//! review and must not create a second one.
//!
//! This is not a nicety. A node that pauses the run with
//! [`NodeControl::Interrupt`](crate::nodes::NodeControl::Interrupt) has its
//! state update discarded, so on resume it re-runs from the top and calls
//! `decide` again — and a polling node calls it once per poll. A provider that
//! created a fresh review per call would spam the human with a new card every
//! time the run looked at the world. The node derives a stable `request_id` from
//! the run and node id (or takes one from config), so honouring it is enough.
//!
//! # Optional, like [`MemoryProvider`](crate::caps::MemoryProvider)
//!
//! Hosts that wire no provider leave [`Capabilities::approvals`](crate::caps::Capabilities::approvals)
//! `None`. An `approval` node then falls back to the engine's existing
//! pause/resume channel: it interrupts the run naming itself on
//! [`RunOutcome::pending_approvals`](crate::engine::RunOutcome::pending_approvals),
//! and the host settles it out of band with
//! [`engine::resume`](crate::engine::resume). That fallback is deliberate —
//! a host that already has a review surface bolted onto run resumption should
//! not have to implement a trait to use this node.

use async_trait::async_trait;
use serde_json::Value;

use crate::error::Result;

/// What the human is being asked to look at.
///
/// `kind` is an **opaque, host-defined** rendering hint (the model layer never
/// interprets it), following the `scope` precedent in
/// [`MemoryProvider`](crate::caps::MemoryProvider). The conventional values are
/// `"url"`, `"text"`, `"markdown"`, and `"json"`; a host is free to define more
/// and a host that renders everything the same way may ignore it entirely.
#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalSubject {
    /// Rendering hint — `"url"` / `"text"` / `"markdown"` / `"json"` by
    /// convention, host-defined in general.
    pub kind: String,
    /// The thing itself: the URL string, the prose, the payload.
    pub value: Value,
}

/// One review request handed to the host.
#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalRequest {
    /// Stable identity of *this* review, and the key the create-or-fetch
    /// contract is built on (see the module docs). Derived from the run and
    /// node id unless the node's `config.request_id` overrides it, so it is the
    /// same string across an interrupt/resume and across every poll.
    pub request_id: String,
    /// The node asking, for a host that wants to link the review back to the
    /// graph.
    pub node_id: String,
    /// The run this review belongs to, when the run state carries one.
    pub run_id: Option<String>,
    /// Short human-facing headline (`config.title`).
    pub title: Option<String>,
    /// Fuller ask — what approving actually authorizes (`config.prompt`).
    pub prompt: Option<String>,
    /// What is being reviewed.
    pub subject: ApprovalSubject,
    /// Opaque host-resolved reviewer handles (user ids, emails, a channel, a
    /// role name). The crate never interprets these.
    pub assignees: Vec<String>,
    /// Anything else the graph attached for the host's benefit
    /// (`config.metadata`), passed through untouched.
    pub metadata: Value,
}

/// A human's verdict on an [`ApprovalRequest`].
#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalDecision {
    /// `true` for approve, `false` for reject. Binary on purpose: an
    /// n-way review is a `switch` on data, not an approval gate.
    pub approved: bool,
    /// Opaque host handle for whoever decided.
    pub decided_by: Option<String>,
    /// Free-text note the reviewer left, surfaced on the emitted item so a
    /// rejection branch can act on the reason.
    pub comment: Option<String>,
    /// The subject as the human left it, when the host's review surface lets
    /// them edit before approving. `None` means "unchanged", and the node emits
    /// the subject it sent.
    pub payload: Option<Value>,
}

impl ApprovalDecision {
    /// An approval with no reviewer, comment, or edit recorded.
    #[must_use]
    pub fn approved() -> Self {
        Self {
            approved: true,
            decided_by: None,
            comment: None,
            payload: None,
        }
    }

    /// A rejection carrying an optional reason.
    #[must_use]
    pub fn rejected(comment: Option<String>) -> Self {
        Self {
            approved: false,
            decided_by: None,
            comment,
            payload: None,
        }
    }
}

/// Where a review stands when the host is asked.
#[derive(Debug, Clone, PartialEq)]
pub enum ApprovalOutcome {
    /// Nobody has decided yet. The node waits — by suspending the run or by
    /// polling, per its `wait_mode`.
    Pending,
    /// A human decided.
    Decided(ApprovalDecision),
}

/// Host-implemented delivery of an approve/reject decision to a human.
///
/// See the module docs for the create-or-fetch contract every implementation
/// must honour, and for what happens on hosts that wire no provider at all.
#[async_trait]
pub trait ApprovalProvider: Send + Sync {
    /// Registers `request` for human review, or — if a review with that
    /// `request_id` already exists — returns where that one stands.
    ///
    /// Must be idempotent on [`ApprovalRequest::request_id`]: this is called
    /// again on every poll and after every resume, and a provider that creates a
    /// new review per call notifies the human once per call.
    ///
    /// # Errors
    /// Returns an [`EngineError::Capability`](crate::error::EngineError::Capability)
    /// when the review cannot be created or read.
    async fn decide(&self, request: &ApprovalRequest) -> Result<ApprovalOutcome>;

    /// Withdraws a review nobody will wait on any more (the node timed out, the
    /// run was cancelled), so a stale card does not sit in a human's queue.
    ///
    /// Best-effort by design: the default implementation does nothing, and the
    /// node logs rather than fails when this errors — the run has already
    /// decided what to do by the time it is called.
    ///
    /// # Errors
    /// Returns an [`EngineError::Capability`](crate::error::EngineError::Capability)
    /// when the host knows the withdrawal failed.
    async fn cancel(&self, request_id: &str, reason: &str) -> Result<()> {
        let _ = (request_id, reason);
        Ok(())
    }
}
