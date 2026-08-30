//! Builds [`Capabilities`] bundles wired to the mock implementations in
//! [`super`] and [`super::mock_approvals`].
//!
//! Split out of `mock.rs` to keep that file under the repository's
//! line-length limit; these are all thin variations on one bundle, so they
//! belong together rather than split further.

use std::sync::Arc;

use crate::caps::{AgentRunner, ApprovalProvider, Capabilities, MemoryProvider, WorkflowResolver};

use super::{
    MockApprovals, MockCode, MockHttp, MockLlm, MockMemory, MockShell, MockStateStore, MockTools,
    MockWorkflowResolver,
};

/// Builds a [`Capabilities`] bundle wired entirely to the mock implementations.
///
/// The bundled [`MockWorkflowResolver`] is empty; use
/// [`mock_capabilities_with_resolver`] to supply one that resolves ids. Unlike
/// [`Capabilities::agent`] (which defaults `None`), [`Capabilities::memory`] is
/// wired to [`MockMemory`] by default — a `memory` node must dry-run
/// successfully out of the box; use `Capabilities { memory: None, ..caps }` to
/// exercise the "host wired no memory store" error path instead.
#[must_use]
pub fn mock_capabilities() -> Capabilities {
    mock_capabilities_with_resolver(MockWorkflowResolver::default())
}

/// Like [`mock_capabilities`], but with a caller-supplied [`WorkflowResolver`]
/// so tests can exercise `sub_workflow`-by-id.
#[must_use]
pub fn mock_capabilities_with_resolver(resolver: impl WorkflowResolver + 'static) -> Capabilities {
    Capabilities {
        llm: Arc::new(MockLlm),
        tools: Arc::new(MockTools),
        http: Arc::new(MockHttp),
        code: Arc::new(MockCode),
        shell: Some(Arc::new(MockShell)),
        state: Arc::new(MockStateStore::default()),
        resolver: Arc::new(resolver),
        // No agent registry by default: `agent` nodes use `MockLlm`. Use
        // [`mock_capabilities_with_agent`] to exercise the `agent_ref` path.
        agent: None,
        // Wired by default (unlike `agent`) — see the doc comment above.
        memory: Some(Arc::new(MockMemory)),
        // The real tokio-backed runner, so `spawn`/`gate` behave under test the
        // way they behave for a host that wires nothing.
        tasks: Some(Arc::new(crate::caps::TokioTaskRunner::new())),
        // Wired by default, like `memory`: a graph with an `approval` node must
        // dry-run without the host standing up a review surface first.
        approvals: Some(Arc::new(MockApprovals::approving())),
    }
}

/// Like [`mock_capabilities`], but wires an [`AgentRunner`] so tests can exercise
/// an `agent` node that selects a named agent kind via `agent_ref`.
#[must_use]
pub fn mock_capabilities_with_agent(agent: impl AgentRunner + 'static) -> Capabilities {
    Capabilities {
        agent: Some(Arc::new(agent)),
        ..mock_capabilities()
    }
}

/// Like [`mock_capabilities`], but with a caller-supplied [`MemoryProvider`] in
/// place of the default [`MockMemory`] — for tests that need custom recall /
/// flavour / people / remember / forget behavior.
#[must_use]
pub fn mock_capabilities_with_memory(memory: impl MemoryProvider + 'static) -> Capabilities {
    Capabilities {
        memory: Some(Arc::new(memory)),
        ..mock_capabilities()
    }
}

/// Like [`mock_capabilities`], but with a caller-supplied [`ApprovalProvider`]
/// in place of the default approve-everything [`MockApprovals`] — for tests
/// that need a rejection, a pending review, or a host-shaped decision.
#[must_use]
pub fn mock_capabilities_with_approvals(
    approvals: impl ApprovalProvider + 'static,
) -> Capabilities {
    Capabilities {
        approvals: Some(Arc::new(approvals)),
        ..mock_capabilities()
    }
}
