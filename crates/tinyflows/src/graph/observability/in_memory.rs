use super::*;

impl InMemoryGraphEventJournal {
    /// Creates a new, empty in-memory journal.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the number of observations stored for `run_id`.
    pub fn len(&self, run_id: &str) -> usize {
        self.runs
            .lock()
            .expect("InMemoryGraphEventJournal lock poisoned")
            .get(run_id)
            .map(|v| v.len())
            .unwrap_or(0)
    }

    /// Returns `true` when no observations are stored for `run_id`.
    pub fn is_empty(&self, run_id: &str) -> bool {
        self.len(run_id) == 0
    }
}

#[async_trait]
impl GraphEventJournal for InMemoryGraphEventJournal {
    async fn append(&self, obs: GraphObservation) -> Result<u64> {
        let mut runs = self
            .runs
            .lock()
            .map_err(|e| poisoned("InMemoryGraphEventJournal", e))?;
        let entries = runs.entry(obs.run_id.as_str().to_string()).or_default();
        let offset = entries.len() as u64;
        entries.push(obs);
        Ok(offset)
    }

    async fn read_from(&self, run_id: &str, offset: u64) -> Result<Vec<GraphObservation>> {
        let runs = self
            .runs
            .lock()
            .map_err(|e| poisoned("InMemoryGraphEventJournal", e))?;
        let Some(entries) = runs.get(run_id) else {
            return Ok(Vec::new());
        };
        Ok(entries.iter().skip(offset as usize).cloned().collect())
    }
}

// ---------------------------------------------------------------------------
// InMemoryGraphStatusStore
// ---------------------------------------------------------------------------

impl InMemoryGraphStatusStore {
    /// Creates a new, empty, **unbounded** in-memory status store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Caps the store at `max` distinct runs.
    ///
    /// Once a `put_status` for a *new* run pushes the store past the cap, the
    /// oldest **terminal** run (completed / failed / cancelled) is evicted
    /// first; when every retained run is still live, the oldest run overall is
    /// evicted. Overwriting an already-recorded run never triggers eviction. A
    /// `max` of `0` retains nothing. The default (via [`Self::new`] /
    /// [`Default`]) is unbounded.
    pub fn with_max_runs(mut self, max: usize) -> Self {
        self.max_runs = Some(max);
        self
    }

    /// Returns the number of distinct runs with a recorded status.
    pub fn len(&self) -> usize {
        self.state
            .lock()
            .expect("InMemoryGraphStatusStore lock poisoned")
            .statuses
            .len()
    }

    /// Returns `true` when no statuses have been recorded.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl StatusStoreState {
    /// Removes `run_id` from the thread index, dropping the thread's bucket
    /// when it becomes empty.
    fn unindex_thread(&mut self, thread_id: &str, run_id: &str) {
        if let Some(runs) = self.by_thread.get_mut(thread_id) {
            runs.retain(|id| id != run_id);
            if runs.is_empty() {
                self.by_thread.remove(thread_id);
            }
        }
    }

    /// Evicts one run: the oldest terminal run if any, otherwise the oldest
    /// run overall. No-op when the store is empty.
    fn evict_one(&mut self) {
        let idx = self
            .order
            .iter()
            .position(|id| self.statuses.get(id).is_none_or(|s| s.is_terminal()))
            .unwrap_or(0);
        let Some(run_id) = self.order.remove(idx) else {
            return;
        };
        if let Some(evicted) = self.statuses.remove(&run_id)
            && let Some(thread_id) = evicted.thread_id.as_ref()
        {
            self.unindex_thread(thread_id.as_str(), &run_id);
        }
    }
}

#[async_trait]
impl GraphStatusStore for InMemoryGraphStatusStore {
    async fn put_status(&self, status: GraphRunStatus) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|e| poisoned("InMemoryGraphStatusStore", e))?;
        let run_id = status.run_id.as_str().to_string();
        let thread_id = status.thread_id.as_ref().map(|t| t.as_str().to_string());

        match state.statuses.insert(run_id.clone(), status) {
            Some(previous) => {
                // Overwrite: keep the thread index coherent if the thread
                // changed (rare, but cheap to handle).
                let previous_thread = previous.thread_id.as_ref().map(|t| t.as_str().to_string());
                if previous_thread != thread_id {
                    if let Some(old) = previous_thread {
                        state.unindex_thread(&old, &run_id);
                    }
                    if let Some(new) = thread_id {
                        state.by_thread.entry(new).or_default().push(run_id);
                    }
                }
            }
            None => {
                // First status for this run: index it and enforce the cap.
                if let Some(thread) = thread_id {
                    state
                        .by_thread
                        .entry(thread)
                        .or_default()
                        .push(run_id.clone());
                }
                state.order.push_back(run_id);
                if let Some(max) = self.max_runs {
                    while state.statuses.len() > max {
                        state.evict_one();
                    }
                }
            }
        }
        Ok(())
    }

    async fn get_status(&self, run_id: &str) -> Result<Option<GraphRunStatus>> {
        let state = self
            .state
            .lock()
            .map_err(|e| poisoned("InMemoryGraphStatusStore", e))?;
        Ok(state.statuses.get(run_id).cloned())
    }

    async fn list_by_thread(&self, thread_id: &str) -> Result<Vec<GraphRunStatus>> {
        let state = self
            .state
            .lock()
            .map_err(|e| poisoned("InMemoryGraphStatusStore", e))?;
        Ok(state
            .by_thread
            .get(thread_id)
            .map(|runs| {
                runs.iter()
                    .filter_map(|id| state.statuses.get(id).cloned())
                    .collect()
            })
            .unwrap_or_default())
    }
}

// ---------------------------------------------------------------------------
// JournalGraphSink
// ---------------------------------------------------------------------------

impl JournalGraphSink {
    /// Builds a journal sink for `run_id` of `graph_id`. `root_run_id` defaults
    /// to `run_id` (a top-level run) and the namespace is empty; use the
    /// builder methods to set a parent, root, thread, namespace, or downstream
    /// sink.
    pub fn new(journal: Arc<dyn GraphEventJournal>, run_id: RunId, graph_id: GraphId) -> Self {
        let worker = Arc::new(AppendWorker::spawn(
            "graph-journal-sink",
            DEFAULT_DRAIN_CAPACITY,
            move |obs: GraphObservation| {
                let journal = Arc::clone(&journal);
                async move { journal.append(obs).await.map(|_| ()) }
            },
        ));
        Self {
            worker,
            inner: None,
            root_run_id: run_id.clone(),
            run_id,
            parent_run_id: None,
            thread_id: None,
            graph_id,
            namespace: Vec::new(),
            offset: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            step: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    /// Sets the parent and root run ids stamped onto every observation.
    pub fn with_lineage(mut self, parent_run_id: Option<RunId>, root_run_id: RunId) -> Self {
        self.parent_run_id = parent_run_id;
        self.root_run_id = root_run_id;
        self
    }

    /// Sets the thread id stamped onto every observation.
    pub fn with_thread(mut self, thread_id: Option<ThreadId>) -> Self {
        self.thread_id = thread_id;
        self
    }

    /// Sets the checkpoint namespace stamped onto every observation.
    ///
    /// A subgraph sink is given the child namespace here so its observations
    /// carry the nested path.
    pub fn with_namespace(mut self, namespace: Vec<String>) -> Self {
        self.namespace = namespace;
        self
    }

    /// Forwards every event to `inner` in addition to journaling it, so one
    /// configured sink can both persist and broadcast.
    pub fn with_inner(mut self, inner: Arc<dyn GraphEventSink>) -> Self {
        self.inner = Some(inner);
        self
    }

    /// Builds the durable observation for `event`, advancing the offset and the
    /// latest-step trackers.
    fn observe(&self, event: &GraphEvent) -> GraphObservation {
        let offset = self.offset.fetch_add(1, Ordering::Relaxed);
        // Track the latest superstep so events without a step (route, run
        // lifecycle) still carry the step they happened during.
        let step = match event.step() {
            Some(step) => {
                self.step.store(step as u64, Ordering::Relaxed);
                step
            }
            None => self.step.load(Ordering::Relaxed) as usize,
        };
        GraphObservation {
            event_id: EventId::new(format!("{}-{offset}", self.run_id.as_str())),
            run_id: self.run_id.clone(),
            root_run_id: self.root_run_id.clone(),
            parent_run_id: self.parent_run_id.clone(),
            thread_id: self.thread_id.clone(),
            graph_id: self.graph_id.clone(),
            checkpoint_id: checkpoint_of(event),
            namespace: self.namespace.clone(),
            step,
            offset,
            ts_ms: now_ms(),
            event: event.clone(),
        }
    }
}

impl GraphEventSink for JournalGraphSink {
    fn emit(&self, event: GraphEvent) {
        let obs = self.observe(&event);
        // Hand off to the background drain; never block the executor on I/O.
        self.worker.submit(obs);
        if let Some(inner) = &self.inner {
            inner.emit(event);
        }
    }

    fn flush(&self) {
        self.worker.flush();
        if let Some(inner) = &self.inner {
            inner.flush();
        }
    }
}

/// Extracts the checkpoint id a [`GraphEvent::CheckpointSaved`] carries, so the
/// observation envelope can record it directly.
fn checkpoint_of(event: &GraphEvent) -> Option<CheckpointId> {
    match event {
        GraphEvent::CheckpointSaved { checkpoint_id } => Some(checkpoint_id.clone()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

pub(super) fn pop_node_start(
    starts: &mut HashMap<(NodeId, usize), VecDeque<u64>>,
    node: &NodeId,
    step: usize,
) -> Option<u64> {
    let key = (node.clone(), step);
    let queue = starts.get_mut(&key)?;
    let start = queue.pop_front();
    if queue.is_empty() {
        starts.remove(&key);
    }
    start
}

pub(super) fn average(total: u64, count: usize) -> Option<u64> {
    (count > 0).then_some(total / count as u64)
}

pub(super) fn duration_ms(start: SystemTime, end: SystemTime) -> Option<u64> {
    end.duration_since(start)
        .ok()
        .map(|duration| duration.as_millis() as u64)
}

/// Builds a uniform poisoned-lock validation error for the in-memory backends.
fn poisoned<E: std::fmt::Display>(what: &str, err: E) -> crate::graph::error::GraphError {
    crate::graph::error::GraphError::Validation(format!("{what} lock poisoned: {err}"))
}
