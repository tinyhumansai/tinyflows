use super::*;

/// Everything the internal build/run seam needs beyond the workflow, its
/// capabilities, and its observer.
///
/// Private on purpose. The engine has fourteen public entry points that all
/// funnel into `build_and_run`, and every optional run knob added over the
/// years — an injected checkpointer, a caller-chosen thread id, an event
/// journal, a run-metadata overlay, a cancellation token — arrived as another
/// positional argument on both this seam and `build_graph`. That is what put
/// `#[allow(clippy::too_many_arguments)]` on four functions here, and it means
/// the fifteenth knob costs fourteen signature edits before it does anything.
///
/// Collecting them in one struct makes the *next* knob a field. It also keeps
/// every public signature exactly as it was: each entry point constructs one of
/// these from what it was already given.
pub(super) struct RunConfig {
    /// Where the run's checkpoints are persisted.
    pub(super) checkpointer: Arc<dyn Checkpointer<Value>>,
    /// The key the run's state is stored under, and what a resume names.
    pub(super) thread_id: String,
    /// The durable event journal, when a caller wants graph observations
    /// recorded.
    pub(super) journal: Option<Arc<dyn GraphEventJournal>>,
    /// Extra keys merged into the seeded `run` state — today only the
    /// `sub_workflow` depth counters a child run inherits.
    pub(super) run_meta_overlay: Option<Value>,
    /// The run's cooperative-cancellation token.
    pub(super) token: CancellationToken,
    /// The step-debugging hook.
    ///
    /// `None` on every entry point that existed before it, which is what keeps
    /// those paths byte-identical: with no interceptor the engine builds no
    /// [`StepFrame`](crate::interception::StepFrame) and makes no call.
    pub(super) interceptor: Option<Arc<dyn StepInterceptor>>,
}

impl RunConfig {
    /// The default run configuration: a process-local in-memory checkpointer
    /// keyed by the workflow's trigger id, no journal, no overlay, no
    /// cancellation, and no interceptor.
    ///
    /// Exactly what [`run`](super::run) and
    /// [`run_with_observer`](super::run_with_observer) used to assemble inline.
    ///
    /// # Errors
    /// Returns [`EngineError::Validation`] if the workflow has no trigger node
    /// to key the thread on.
    pub(super) fn new(workflow: &CompiledWorkflow) -> Result<Self> {
        Ok(Self {
            checkpointer: Arc::new(InMemoryCheckpointer::<Value>::default()),
            thread_id: default_thread_id(workflow)?,
            journal: None,
            run_meta_overlay: None,
            token: CancellationToken::new(),
            interceptor: None,
        })
    }

    /// Persist to a host-supplied `checkpointer`, keyed by `thread_id`.
    ///
    /// This is what makes a run resumable in a *different process*: the host
    /// rebuilds the same graph, re-attaches the same checkpointer, and names
    /// the same thread.
    #[must_use]
    pub(super) fn with_checkpointer(
        mut self,
        checkpointer: Arc<dyn Checkpointer<Value>>,
        thread_id: &str,
    ) -> Self {
        self.checkpointer = checkpointer;
        self.thread_id = thread_id.to_string();
        self
    }

    /// Record every graph event as a
    /// [`GraphObservation`](crate::graph::GraphObservation) in `journal`.
    #[must_use]
    pub(super) fn with_journal(mut self, journal: Arc<dyn GraphEventJournal>) -> Self {
        self.journal = Some(journal);
        self
    }

    /// Merge `overlay` into the seeded `run` state.
    #[must_use]
    pub(super) fn with_overlay(mut self, overlay: Value) -> Self {
        self.run_meta_overlay = Some(overlay);
        self
    }

    /// Observe `token`, so cancelling it winds the run down at the next node
    /// boundary.
    #[must_use]
    pub(super) fn with_token(mut self, token: CancellationToken) -> Self {
        self.token = token;
        self
    }

    /// Route every node activation through `interceptor` before and after it
    /// executes.
    #[must_use]
    pub(super) fn with_interceptor(mut self, interceptor: Arc<dyn StepInterceptor>) -> Self {
        self.interceptor = Some(interceptor);
        self
    }
}
