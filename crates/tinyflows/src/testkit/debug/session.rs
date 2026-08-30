use std::sync::Arc;
use std::time::Duration;

use futures_util::future::{Either, select};
use tokio::task::JoinHandle;

use crate::caps::Capabilities;
use crate::compiler::CompiledWorkflow;
use crate::engine::{CancellationToken, RunInput, RunOutcome, run_intercepted};
use crate::error::{EngineError, Result};
use crate::observability::{NoopObserver, RunObserver};

use super::controller::{DebugController, PauseSnapshot, PauseStream};

/// Where a debug session has got to.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SessionStatus {
    /// The run is executing, with nothing parked.
    Running,
    /// One or more activations are parked at a breakpoint.
    Paused(usize),
    /// The run finished.
    Finished,
    /// The run failed.
    Failed(String),
}

/// A workflow run owned by a debugger.
///
/// The run is spawned onto its own task, so the caller is free to inspect and
/// step it from somewhere else. That is the part a debugger actually needs and
/// the pause mechanism does not supply: an agent issuing `flow_debug.inspect`
/// and `flow_debug.step` calls in separate turns needs the run to still be
/// there between them, which it cannot be if it is sitting on the caller's
/// stack.
///
/// # Lifetime
///
/// A session lives in one process and dies with it. That is the honest cost of
/// a live pause, and it is the right trade for a debugger: the durable pause the
/// engine already has ([`requires_approval`] gates and checkpointed resume) is
/// for waiting on a *person*, which can take days, while a breakpoint is for
/// waiting on someone who is looking at it right now.
///
/// Dropping a session detaches it, cancels the run, and only then aborts the
/// task — in that order, so a dropped session never leaves an activation parked
/// on a channel whose sender died with the table.
///
/// [`requires_approval`]: crate::engine::RunInput::with_approvals
pub struct DebugSession {
    controller: Arc<DebugController>,
    pauses: PauseStream,
    token: CancellationToken,
    handle: Option<JoinHandle<Result<RunOutcome>>>,
}

impl DebugSession {
    /// Start `workflow` on a spawned task with a debugger attached.
    ///
    /// Returns as soon as the run is spawned — before it has executed anything —
    /// so a caller can set breakpoints and then wait for the first pause.
    ///
    /// Takes its arguments by value because the run outlives this call.
    ///
    /// # Errors
    /// Returns [`EngineError::Capability`] when there is no tokio runtime to
    /// spawn onto, rather than panicking inside `tokio::spawn` — the same
    /// courtesy [`TokioTaskRunner`](crate::caps::TokioTaskRunner) extends.
    pub fn start(
        workflow: CompiledWorkflow,
        input: impl Into<RunInput>,
        capabilities: Capabilities,
        observer: Arc<dyn RunObserver>,
    ) -> Result<Self> {
        if tokio::runtime::Handle::try_current().is_err() {
            return Err(EngineError::Capability(
                "a debug session needs a tokio runtime to spawn its run onto".to_string(),
            ));
        }
        let (controller, pauses) = DebugController::new();
        let token = CancellationToken::new();
        let input = input.into();

        let handle = {
            let controller = controller.clone();
            let token = token.clone();
            tokio::spawn(async move {
                run_intercepted(
                    &workflow,
                    input,
                    &capabilities,
                    &observer,
                    token,
                    controller as Arc<dyn crate::interception::StepInterceptor>,
                )
                .await
                .map(|(outcome, _resumable)| outcome)
            })
        };

        Ok(Self {
            controller,
            pauses,
            token,
            handle: Some(handle),
        })
    }

    /// Start a session with no observer attached.
    ///
    /// # Errors
    /// Same as [`start`](Self::start).
    pub fn start_quiet(
        workflow: CompiledWorkflow,
        input: impl Into<RunInput>,
        capabilities: Capabilities,
    ) -> Result<Self> {
        Self::start(workflow, input, capabilities, Arc::new(NoopObserver))
    }

    /// The controller driving this session: where breakpoints are set and
    /// parked activations are released.
    #[must_use]
    pub fn controller(&self) -> &Arc<DebugController> {
        &self.controller
    }

    /// Where the session has got to.
    #[must_use]
    pub fn status(&self) -> SessionStatus {
        let parked = self.controller.pauses().len();
        if parked > 0 {
            return SessionStatus::Paused(parked);
        }
        match self.handle.as_ref() {
            Some(handle) if !handle.is_finished() => SessionStatus::Running,
            _ => SessionStatus::Finished,
        }
    }

    /// Wait for the next activation to park, giving up after `timeout`.
    ///
    /// `None` means the timeout elapsed or the run ended without parking —
    /// which is a normal outcome, not an error: a breakpoint on a node a
    /// condition routed past never fires.
    pub async fn next_pause(&mut self, timeout: Duration) -> Option<PauseSnapshot> {
        let timer = futures_timer::Delay::new(timeout);
        match select(std::pin::pin!(self.pauses.next()), std::pin::pin!(timer)).await {
            Either::Left((pause, _)) => pause,
            Either::Right(((), _)) => None,
        }
    }

    /// Whether the run task has finished.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.handle.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Wind the run down cooperatively at the next node boundary.
    ///
    /// Detaches first, so anything currently parked is released and can *reach*
    /// that boundary — cancelling a parked run without detaching would leave it
    /// waiting for a decision that is never coming.
    pub fn cancel(&self) {
        self.controller.detach();
        self.token.cancel();
    }

    /// Wait for the run to finish and return its outcome.
    ///
    /// Detaches first so a run parked at a breakpoint is released rather than
    /// waited on forever.
    ///
    /// # Errors
    /// Propagates the run's own error, or [`EngineError::Capability`] if the run
    /// task panicked or was aborted.
    pub async fn finish(mut self) -> Result<RunOutcome> {
        self.controller.detach();
        let handle = self
            .handle
            .take()
            .ok_or_else(|| EngineError::Capability("this session was already finished".into()))?;
        match handle.await {
            Ok(result) => result,
            Err(err) => Err(EngineError::Capability(format!(
                "the debug session's run task did not complete: {err}"
            ))),
        }
    }
}

impl Drop for DebugSession {
    fn drop(&mut self) {
        // Order matters, and is the whole point of writing this by hand.
        //
        // 1. Detach, so no *new* activation parks and every parked one is
        //    released. Without this an abort can land while an activation is
        //    waiting on a channel whose sender is about to be dropped.
        // 2. Cancel, so the run winds down at the next node boundary and gets
        //    to record what it did.
        // 3. Abort, purely as a backstop for a task already past its last
        //    cancellation check.
        self.controller.detach();
        self.token.cancel();
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
