//! A [`Checkpointer`] over `tinystoragedrivers` document ports.
//!
//! [`DriverCheckpointer`] puts graph checkpoints and their pending writes on
//! whatever backend the host opened (SQLite on a desktop, MongoDB in the
//! cloud, memory in tests). The document handle is
//! already bound to a tenant scope, so two tenants' threads never meet even
//! when they share a thread id.
//!
//! # Layout
//!
//! The graph checkpointer exists once in the storage schema, as the `graph.*`
//! collections that `tinyagents-graph` declares and migrates; every user of it
//! tells itself apart by a namespace (`ns`). This crate keeps its own
//! checkpointer code (tinyflows does not depend on tinyagents) but reads and
//! writes exactly those documents, under namespace [`NAMESPACE`] (`flows`) by
//! default; `checkpoint_layout` holds the ids and bodies:
//!
//! - `graph.checkpoints`: one document per stored checkpoint,
//!   `{ns, thread, subgraph, seq, checkpoint_id, checkpoint}` under
//!   `Key::of([ns, thread, seq])`. `subgraph` is the encoded subgraph
//!   namespace, so scoped reads filter on it. `seq` is a per-thread insertion
//!   counter, so listing is a query sorted on it and duplicate checkpoint ids
//!   resolve to the latest write, exactly like the append-only backends.
//! - `graph.threads`: one counter document per thread, `{ns, thread, next}`,
//!   advanced with [`Repo::update`] so concurrent writers never share a `seq`.
//! - `graph.writes`: one document per `(thread, subgraph, checkpoint)` holding
//!   the merged pending writes (see
//!   [`tinyflows::graph::checkpoint::merge_writes`]).
//!
//! The engine's checkpointer trait has no execution lease, so no `tsd.leases`
//! record is written here; a host that fences a thread uses the lease key
//! `Key::of([ns, thread])` under owner `graph`, as `tinyagents-graph` does.

use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tinyflows::graph::error::{GraphError, Result};
use tinyflows::graph::ids::CheckpointId;
use tinyflows::graph::{
    Checkpoint, CheckpointConfig, CheckpointMetadata, CheckpointTuple, Checkpointer, PendingWrite,
};
use tinyflows_storage::{
    DocumentStore, DocumentStoreExt, ErrorKind, Filter, Query, Repo, Sort, Versioned, WriteOptions,
};
use tokio::sync::OnceCell;

use crate::checkpoint_layout::{
    CHECKPOINTS, CheckpointBody, NAMESPACE, SCHEMA_VERSION, THREADS, ThreadBody, WRITES,
    WritesBody, checkpoint_id_key, checkpoints_spec, map_error, require_checkpoint_id,
    subgraph_key, thread_id_key, threads_spec, writes_id_key, writes_spec,
};

/// A [`Checkpointer`] that stores everything in a driver [`DocumentStore`].
pub struct DriverCheckpointer<State> {
    docs: Arc<dyn DocumentStore>,
    ns: String,
    checkpoints: Repo<CheckpointBody>,
    threads: Repo<ThreadBody>,
    writes: Repo<WritesBody>,
    declared: Arc<OnceCell<()>>,
    _state: PhantomData<fn() -> State>,
}

impl<State> Clone for DriverCheckpointer<State> {
    fn clone(&self) -> Self {
        Self {
            docs: Arc::clone(&self.docs),
            ns: self.ns.clone(),
            checkpoints: self.checkpoints.clone(),
            threads: self.threads.clone(),
            writes: self.writes.clone(),
            declared: Arc::clone(&self.declared),
            _state: PhantomData,
        }
    }
}

impl<State> DriverCheckpointer<State> {
    /// Store checkpoints in the `graph.*` collections of `docs`, under the
    /// workflow namespace [`NAMESPACE`] (`flows`).
    pub fn new(docs: Arc<dyn DocumentStore>) -> Self {
        Self::with_namespace(docs, NAMESPACE)
    }

    /// Store checkpoints in the `graph.*` collections of `docs` under the
    /// namespace `ns`, so several independent checkpointers can share one
    /// backend without meeting.
    ///
    /// # Panics
    ///
    /// Never: the collection names are valid constants.
    #[allow(clippy::expect_used, reason = "the collection names are constants")]
    pub fn with_namespace(docs: Arc<dyn DocumentStore>, ns: impl Into<String>) -> Self {
        let options = WriteOptions::new().schema(SCHEMA_VERSION);
        fn repo<T>(docs: &Arc<dyn DocumentStore>, name: &str, options: WriteOptions) -> Repo<T> {
            Repo::new(docs, name)
                .expect("graph collections are valid names")
                .with_options(options)
        }
        Self {
            checkpoints: repo(&docs, CHECKPOINTS, options),
            threads: repo(&docs, THREADS, options),
            writes: repo(&docs, WRITES, options),
            docs,
            ns: ns.into(),
            declared: Arc::new(OnceCell::new()),
            _state: PhantomData,
        }
    }

    /// This checkpointer's namespace.
    pub fn namespace(&self) -> &str {
        &self.ns
    }

    async fn declared(&self) -> Result<()> {
        self.declared
            .get_or_try_init(|| async {
                for spec in [checkpoints_spec(), threads_spec(), writes_spec()] {
                    self.docs
                        .ensure_collection(&spec)
                        .await
                        .map_err(map_error)?;
                }
                Ok::<(), GraphError>(())
            })
            .await
            .map(|_| ())
    }

    /// Reserve the next insertion sequence number for `thread`.
    async fn next_seq(&self, thread: &str) -> Result<u64> {
        let (stored, _) = self
            .threads
            .update(&thread_id_key(&self.ns, thread), |current| {
                Ok(Some(ThreadBody {
                    ns: self.ns.clone(),
                    thread: thread.to_owned(),
                    next: current.map_or(1, |counter| counter.next + 1),
                }))
            })
            .await
            .map_err(map_error)?
            .ok_or_else(|| {
                GraphError::Checkpoint(format!(
                    "could not reserve a checkpoint sequence for thread `{thread}`"
                ))
            })?;
        Ok(stored.next - 1)
    }

    /// The filter selecting `thread`'s documents in this namespace.
    fn thread_filter(&self, thread: &str) -> Filter {
        Filter::eq("ns", self.ns.as_str()).and(Filter::eq("thread", thread))
    }

    /// Every stored checkpoint document of `thread`, in insertion order.
    async fn thread_docs(&self, thread: &str) -> Result<Vec<Versioned<Value>>> {
        self.declared().await?;
        let query = Query::filter(self.thread_filter(thread)).sort(Sort::asc("seq"));
        self.docs
            .query_all(CHECKPOINTS, &query)
            .await
            .map_err(map_error)
    }

    async fn drop_writes(&self, filter: Filter) -> Result<()> {
        self.docs
            .delete_where(WRITES, &filter)
            .await
            .map(|_| ())
            .map_err(map_error)
    }

    /// The newest checkpoint document matching `filter`.
    async fn newest(&self, filter: Filter) -> Result<Option<Versioned<Value>>> {
        let query = Query::filter(filter).sort(Sort::desc("seq")).limit(1);
        let page = self
            .docs
            .query(CHECKPOINTS, &query)
            .await
            .map_err(map_error)?;
        Ok(page.items.into_iter().next())
    }
}

impl<State> DriverCheckpointer<State>
where
    State: DeserializeOwned,
{
    fn decode(stored: Versioned<Value>) -> Result<Checkpoint<State>> {
        let record = stored.doc.get("checkpoint").cloned().unwrap_or(Value::Null);
        serde_json::from_value(record).map_err(|error| {
            GraphError::Checkpoint(format!(
                "storage driver checkpointer: decode record: {error}"
            ))
        })
    }
}

#[async_trait]
impl<State> Checkpointer<State> for DriverCheckpointer<State>
where
    State: Serialize + DeserializeOwned + Send + Sync + 'static,
{
    async fn put(&self, checkpoint: Checkpoint<State>) -> Result<CheckpointId> {
        self.declared().await?;
        let id = CheckpointId::new(checkpoint.checkpoint_id.clone());
        let seq = self.next_seq(&checkpoint.thread_id).await?;
        let body = CheckpointBody {
            ns: self.ns.clone(),
            thread: checkpoint.thread_id.clone(),
            subgraph: subgraph_key(&checkpoint.namespace),
            seq,
            checkpoint_id: checkpoint.checkpoint_id.clone(),
            checkpoint: serde_json::to_value(&checkpoint)?,
        };
        self.checkpoints
            .insert(
                &checkpoint_id_key(&self.ns, &checkpoint.thread_id, seq),
                &body,
            )
            .await
            .map_err(map_error)?;
        Ok(id)
    }

    async fn get(
        &self,
        thread_id: &str,
        checkpoint_id: Option<&str>,
    ) -> Result<Option<Checkpoint<State>>> {
        self.declared().await?;
        let mut filter = self.thread_filter(thread_id);
        if let Some(id) = checkpoint_id {
            filter = filter.and(Filter::eq("checkpoint_id", id));
        }
        self.newest(filter).await?.map(Self::decode).transpose()
    }

    /// One indexed query on the stored subgraph namespace, so a parent run and
    /// a subgraph sharing a thread never load each other's checkpoints, even
    /// when they reuse a checkpoint id.
    async fn get_scoped(
        &self,
        thread_id: &str,
        checkpoint_id: Option<&str>,
        namespace: &[String],
    ) -> Result<Option<Checkpoint<State>>> {
        self.declared().await?;
        let mut filter = self
            .thread_filter(thread_id)
            .and(Filter::eq("subgraph", subgraph_key(namespace)));
        if let Some(id) = checkpoint_id {
            filter = filter.and(Filter::eq("checkpoint_id", id));
        }
        self.newest(filter).await?.map(Self::decode).transpose()
    }

    /// One indexed read of the namespace's checkpoints and one of its
    /// pending writes, then the lineage walk in memory — instead of the
    /// trait default's `get_scoped` plus `get_writes` per hop, which on a
    /// remote backend is two round trips per ancestor.
    async fn state_history(
        &self,
        thread_id: &str,
        namespace: &[String],
        limit: Option<usize>,
    ) -> Result<Vec<CheckpointTuple<State>>> {
        self.declared().await?;
        let subgraph = subgraph_key(namespace);
        let filter = self
            .thread_filter(thread_id)
            .and(Filter::eq("subgraph", subgraph.as_str()));
        let records = self
            .docs
            .query_all(
                CHECKPOINTS,
                &Query::filter(filter.clone()).sort(Sort::asc("seq")),
            )
            .await
            .map_err(map_error)?;
        if records.is_empty() {
            return Ok(Vec::new());
        }
        let mut writes: HashMap<String, Vec<PendingWrite>> = HashMap::new();
        let stored_writes = self
            .docs
            .query_all(
                WRITES,
                &Query::filter(
                    self.thread_filter(thread_id)
                        .and(Filter::eq("subgraph", subgraph.as_str())),
                ),
            )
            .await
            .map_err(map_error)?;
        for stored in stored_writes {
            let Some(id) = stored.doc.get("checkpoint_id").and_then(Value::as_str) else {
                continue;
            };
            let batch: Vec<PendingWrite> =
                serde_json::from_value(stored.doc.get("writes").cloned().unwrap_or(Value::Null))?;
            writes.insert(id.to_string(), batch);
        }

        // Last write wins for a re-used id, matching `get`.
        let mut by_id: HashMap<String, Checkpoint<State>> = HashMap::with_capacity(records.len());
        let mut cursor: Option<String> = None;
        for stored in records {
            let checkpoint = Self::decode(stored)?;
            cursor = Some(checkpoint.checkpoint_id.clone());
            by_id.insert(checkpoint.checkpoint_id.clone(), checkpoint);
        }

        let mut out = Vec::new();
        while let Some(id) = cursor {
            if limit.is_some_and(|limit| out.len() >= limit) {
                break;
            }
            // `remove` doubles as the cycle guard: each id is visited once.
            let Some(checkpoint) = by_id.remove(&id) else {
                break;
            };
            cursor = checkpoint.parent_checkpoint_id.clone();
            let config = CheckpointConfig {
                thread_id: checkpoint.thread_id.clone(),
                checkpoint_id: Some(checkpoint.checkpoint_id.clone()),
                namespace: checkpoint.namespace.clone(),
            };
            let parent_config =
                checkpoint
                    .parent_checkpoint_id
                    .as_ref()
                    .map(|parent| CheckpointConfig {
                        thread_id: checkpoint.thread_id.clone(),
                        checkpoint_id: Some(parent.clone()),
                        namespace: checkpoint.namespace.clone(),
                    });
            // The persisted write ledger when there is one, the record's own
            // inline writes otherwise — `resolved_writes`' rule.
            let pending_writes = writes
                .remove(&checkpoint.checkpoint_id)
                .filter(|batch| !batch.is_empty())
                .unwrap_or_else(|| checkpoint.pending_writes.clone());
            out.push(CheckpointTuple {
                config,
                checkpoint,
                parent_config,
                pending_writes,
            });
        }
        Ok(out)
    }

    async fn list(&self, thread_id: &str) -> Result<Vec<CheckpointMetadata>> {
        Ok(self
            .get_thread(thread_id)
            .await?
            .iter()
            .map(Checkpoint::to_metadata)
            .collect())
    }

    async fn get_thread(&self, thread_id: &str) -> Result<Vec<Checkpoint<State>>> {
        self.thread_docs(thread_id)
            .await?
            .into_iter()
            .map(Self::decode)
            .collect()
    }

    async fn list_threads(&self) -> Result<Vec<String>> {
        self.declared().await?;
        let counters = self
            .docs
            .query_all(THREADS, &Query::filter(Filter::eq("ns", self.ns.as_str())))
            .await
            .map_err(map_error)?;
        let mut threads = Vec::new();
        for counter in counters {
            let Some(thread) = counter.doc.get("thread").and_then(Value::as_str) else {
                continue;
            };
            let live = self
                .docs
                .count(CHECKPOINTS, &self.thread_filter(thread))
                .await
                .map_err(map_error)?;
            if live > 0 {
                threads.push(thread.to_owned());
            }
        }
        Ok(threads)
    }

    async fn delete_thread(&self, thread_id: &str) -> Result<()> {
        self.declared().await?;
        self.docs
            .delete_where(CHECKPOINTS, &self.thread_filter(thread_id))
            .await
            .map_err(map_error)?;
        // Keep the sequence counter: a later thread of the same name continues
        // after it, so its records never interleave with stale cursors.
        self.drop_writes(self.thread_filter(thread_id)).await
    }

    async fn delete_checkpoints(&self, thread_id: &str, ids: &[String]) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        self.declared().await?;
        let filter = self.thread_filter(thread_id).and(Filter::one_of(
            "checkpoint_id",
            ids.iter().map(String::as_str),
        ));
        let removed = self
            .docs
            .delete_where(CHECKPOINTS, &filter)
            .await
            .map_err(map_error)?;
        self.drop_writes(filter).await?;
        Ok(usize::try_from(removed).unwrap_or(usize::MAX))
    }

    async fn put_writes(&self, config: &CheckpointConfig, writes: &[PendingWrite]) -> Result<()> {
        checkpoint_import::put_live_writes(self, config, writes).await
    }

    async fn get_writes(&self, config: &CheckpointConfig) -> Result<Vec<PendingWrite>> {
        let Some(checkpoint_id) = self.resolve_write_target(config).await? else {
            return Ok(Vec::new());
        };
        self.declared().await?;
        let id = writes_id_key(
            &self.ns,
            &config.thread_id,
            &subgraph_key(&config.namespace),
            &checkpoint_id,
        );
        Ok(self
            .writes
            .get(&id)
            .await
            .map_err(map_error)?
            .map(|found| found.doc.writes)
            .unwrap_or_default())
    }
}

#[path = "checkpoint_import.rs"]
mod checkpoint_import;
#[cfg(test)]
#[path = "checkpoint_history_tests.rs"]
mod history_tests;
#[cfg(test)]
#[path = "checkpoint_lineage_tests.rs"]
mod lineage_tests;
#[cfg(test)]
#[path = "checkpoint_tests.rs"]
mod tests;

#[path = "checkpoint_debug.rs"]
mod checkpoint_debug;
