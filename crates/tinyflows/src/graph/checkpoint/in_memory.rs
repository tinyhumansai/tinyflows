use super::*;

impl<State> InMemoryCheckpointer<State> {
    /// Creates an empty checkpointer.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            writes: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Returns the number of checkpoints stored for a thread.
    pub fn count(&self, thread_id: &str) -> usize {
        self.inner
            .lock()
            .map(|m| m.get(thread_id).map(|v| v.len()).unwrap_or(0))
            .unwrap_or(0)
    }
}

impl<State> Default for InMemoryCheckpointer<State> {
    fn default() -> Self {
        Self::new()
    }
}

impl<State> Clone for InMemoryCheckpointer<State> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            writes: self.writes.clone(),
        }
    }
}

fn lock_err() -> GraphError {
    GraphError::Checkpoint("in-memory checkpointer lock poisoned".to_string())
}

fn metadata_of<State>(c: &Checkpoint<State>) -> CheckpointMetadata {
    c.to_metadata()
}

#[async_trait]
impl<State> Checkpointer<State> for InMemoryCheckpointer<State>
where
    State: Clone + Send + Sync + 'static,
{
    async fn put(&self, checkpoint: Checkpoint<State>) -> Result<CheckpointId> {
        let id = CheckpointId::new(checkpoint.checkpoint_id.clone());
        let mut map = self.inner.lock().map_err(|_| lock_err())?;
        map.entry(checkpoint.thread_id.clone())
            .or_default()
            .push(checkpoint);
        Ok(id)
    }

    async fn get(
        &self,
        thread_id: &str,
        checkpoint_id: Option<&str>,
    ) -> Result<Option<Checkpoint<State>>> {
        let map = self.inner.lock().map_err(|_| lock_err())?;
        let Some(list) = map.get(thread_id) else {
            return Ok(None);
        };
        // Duplicate-id lookup resolves to the *last* written record, matching
        // the append-only file/sqlite backends (and `get(None)`, which returns
        // the latest). Pinning one semantic keeps the three backends
        // interchangeable — see the checkpointer conformance suite.
        let found = match checkpoint_id {
            Some(id) => list.iter().rfind(|c| c.checkpoint_id == id),
            None => list.last(),
        };
        Ok(found.cloned())
    }

    async fn list(&self, thread_id: &str) -> Result<Vec<CheckpointMetadata>> {
        let map = self.inner.lock().map_err(|_| lock_err())?;
        Ok(map
            .get(thread_id)
            .map(|list| list.iter().map(metadata_of).collect())
            .unwrap_or_default())
    }

    async fn get_thread(&self, thread_id: &str) -> Result<Vec<Checkpoint<State>>> {
        // Single-pass bulk read: clone the thread's records in insertion
        // order, instead of the default's one `get` per listed id.
        let map = self.inner.lock().map_err(|_| lock_err())?;
        Ok(map.get(thread_id).cloned().unwrap_or_default())
    }

    async fn list_threads(&self) -> Result<Vec<String>> {
        let map = self.inner.lock().map_err(|_| lock_err())?;
        Ok(map
            .iter()
            .filter(|(_, list)| !list.is_empty())
            .map(|(thread, _)| thread.clone())
            .collect())
    }

    async fn delete_thread(&self, thread_id: &str) -> Result<()> {
        let mut map = self.inner.lock().map_err(|_| lock_err())?;
        map.remove(thread_id);
        // Writes are keyed by thread too, and deleting a thread must not leave
        // its write ledger behind for a later thread of the same name to
        // inherit. The conformance suite asserts exactly this.
        let mut writes = self.writes.lock().map_err(|_| lock_err())?;
        writes.retain(|(thread, _, _), _| thread != thread_id);
        Ok(())
    }

    async fn delete_checkpoints(&self, thread_id: &str, ids: &[String]) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let drop: HashSet<&str> = ids.iter().map(String::as_str).collect();
        let mut map = self.inner.lock().map_err(|_| lock_err())?;
        let Some(list) = map.get_mut(thread_id) else {
            return Ok(0);
        };
        let before = list.len();
        list.retain(|c| !drop.contains(c.checkpoint_id.as_str()));
        let removed = before - list.len();
        drop_writes_for(&self.writes, thread_id, &drop)?;
        Ok(removed)
    }

    async fn put_writes(&self, config: &CheckpointConfig, writes: &[PendingWrite]) -> Result<()> {
        let checkpoint_id = require_checkpoint_id(config)?;
        if writes.is_empty() {
            return Ok(());
        }
        let key: WriteKey = (
            config.thread_id.clone(),
            config.namespace.clone(),
            checkpoint_id,
        );
        let mut map = self.writes.lock().map_err(|_| lock_err())?;
        let slot = map.entry(key).or_default();
        let changed = merge_writes(slot, writes);
        tracing::debug!(
            "[checkpoint:memory] put_writes thread={} checkpoint={:?} offered={} stored={}",
            config.thread_id,
            config.checkpoint_id,
            writes.len(),
            changed
        );
        Ok(())
    }

    async fn get_writes(&self, config: &CheckpointConfig) -> Result<Vec<PendingWrite>> {
        let Some(checkpoint_id) = self.resolve_write_target(config).await? else {
            return Ok(Vec::new());
        };
        let key: WriteKey = (
            config.thread_id.clone(),
            config.namespace.clone(),
            checkpoint_id,
        );
        let map = self.writes.lock().map_err(|_| lock_err())?;
        Ok(map.get(&key).cloned().unwrap_or_default())
    }
}

/// Removes the write ledgers of `ids` within `thread_id`.
fn drop_writes_for(
    writes: &Mutex<HashMap<WriteKey, Vec<PendingWrite>>>,
    thread_id: &str,
    ids: &HashSet<&str>,
) -> Result<()> {
    let mut map = writes.lock().map_err(|_| lock_err())?;
    map.retain(|(thread, _, checkpoint), _| {
        thread != thread_id || !ids.contains(checkpoint.as_str())
    });
    Ok(())
}

/// Extracts the checkpoint id a `put_writes` call addresses.
///
/// A write belongs to the boundary that produced it, so an unaddressed
/// (`None`) id is a caller bug rather than "the latest": silently filing the
/// writes against whatever checkpoint happens to be newest is precisely the
/// corruption the protocol exists to prevent.
pub(crate) fn require_checkpoint_id(config: &CheckpointConfig) -> Result<String> {
    config.checkpoint_id.clone().ok_or_else(|| {
        GraphError::Checkpoint(format!(
            "put_writes requires an explicit checkpoint_id (thread `{}`)",
            config.thread_id
        ))
    })
}
