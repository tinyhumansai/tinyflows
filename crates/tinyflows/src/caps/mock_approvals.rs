//! [`MockApprovals`], the in-memory [`ApprovalProvider`] used by
//! [`mock_capabilities`](super::mock::mock_capabilities) and by tests that want
//! to drive the `approval` node's approve / reject / pending paths without a
//! real review surface.

use async_trait::async_trait;

use crate::caps::{ApprovalDecision, ApprovalOutcome, ApprovalProvider, ApprovalRequest};
use crate::error::Result;

/// An [`ApprovalProvider`] that answers every review the same way, without a
/// human anywhere.
///
/// Defaults to approving, so a graph containing an `approval` node dry-runs
/// end-to-end out of the box (the `MockMemory` precedent). Use
/// [`rejecting`](Self::rejecting) to drive the reject branch and
/// [`pending`](Self::pending) to exercise the waiting path — a suspending node
/// then pauses the run, and a polling one spends its poll budget.
///
/// Records every `request_id` it has seen so a test can assert the
/// create-or-fetch contract holds (one review per id, however many activations
/// the node had).
#[derive(Debug, Default)]
pub struct MockApprovals {
    outcome: MockApprovalOutcome,
    seen: std::sync::Mutex<Vec<String>>,
    cancelled: std::sync::Mutex<Vec<String>>,
}

/// What a [`MockApprovals`] answers with.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum MockApprovalOutcome {
    /// Approve immediately.
    #[default]
    Approve,
    /// Reject immediately.
    Reject,
    /// Never decide.
    Pending,
}

impl MockApprovals {
    /// A provider that approves every request.
    #[must_use]
    pub fn approving() -> Self {
        Self::default()
    }

    /// A provider that rejects every request, with `"mock rejection"` as the
    /// reviewer's comment.
    #[must_use]
    pub fn rejecting() -> Self {
        Self {
            outcome: MockApprovalOutcome::Reject,
            ..Self::default()
        }
    }

    /// A provider that leaves every request pending forever.
    #[must_use]
    pub fn pending() -> Self {
        Self {
            outcome: MockApprovalOutcome::Pending,
            ..Self::default()
        }
    }

    /// Every `request_id` passed to [`ApprovalProvider::decide`], in call order
    /// (with repeats — the point is to show repeats are the *same* id).
    #[must_use]
    pub fn requested(&self) -> Vec<String> {
        self.seen.lock().expect("lock").clone()
    }

    /// Every `request_id` passed to [`ApprovalProvider::cancel`].
    #[must_use]
    pub fn cancelled(&self) -> Vec<String> {
        self.cancelled.lock().expect("lock").clone()
    }
}

#[async_trait]
impl ApprovalProvider for MockApprovals {
    async fn decide(&self, request: &ApprovalRequest) -> Result<ApprovalOutcome> {
        self.seen
            .lock()
            .expect("lock")
            .push(request.request_id.clone());
        Ok(match self.outcome {
            MockApprovalOutcome::Approve => ApprovalOutcome::Decided(ApprovalDecision {
                decided_by: Some("mock-reviewer".to_string()),
                ..ApprovalDecision::approved()
            }),
            MockApprovalOutcome::Reject => ApprovalOutcome::Decided(ApprovalDecision {
                decided_by: Some("mock-reviewer".to_string()),
                ..ApprovalDecision::rejected(Some("mock rejection".to_string()))
            }),
            MockApprovalOutcome::Pending => ApprovalOutcome::Pending,
        })
    }

    async fn cancel(&self, request_id: &str, _reason: &str) -> Result<()> {
        self.cancelled
            .lock()
            .expect("lock")
            .push(request_id.to_string());
        Ok(())
    }
}
