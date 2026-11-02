//! One owner-and-flow-scoped critical section for run settlement of `dedup`
//! node state.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, Weak};

use serde_json::Value;
use tinyflows::{
    model::{NodeKind, WorkflowGraph},
    nodes::control_flow::dedup_settle,
};
use tinyflows_bus::runs::RunContext;
use tinyflows_drivers::catalog::{FlowCatalogDocuments, FlowStateDocuments};
use tokio::sync::OwnedMutexGuard;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct SettlementKey {
    destination: String,
    object_path: String,
    flow_id: String,
}

static SETTLEMENT_LOCKS: LazyLock<Mutex<HashMap<SettlementKey, Weak<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(Default::default);

fn lock_for(key: SettlementKey) -> Arc<tokio::sync::Mutex<()>> {
    let mut locks = SETTLEMENT_LOCKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    locks.insert(key, Arc::downgrade(&lock));
    lock
}

/// Holds the per-flow settlement lease until the caller finishes all terminal
/// cleanup (checkpoint deletion and terminal event persistence).
pub(super) struct DedupSettlementScope {
    flow_id: String,
    run_id: String,
    node_ids: Vec<String>,
    _guard: Option<OwnedMutexGuard<()>>,
}

impl DedupSettlementScope {
    /// Acquires the settlement scope for the exact CatalogStorage owner and
    /// the run's flow. Graphs without a `dedup` node require no lease.
    pub(super) async fn acquire(
        storage: &tinyflows_bus::catalog::CatalogStorage,
        context: &RunContext,
        graph: &WorkflowGraph,
        run_id: &str,
    ) -> Self {
        let node_ids: Vec<String> = graph
            .nodes
            .iter()
            .filter(|node| node.kind == NodeKind::Dedup)
            .map(|node| node.id.clone())
            .collect();
        let guard = if node_ids.is_empty() {
            None
        } else {
            Some(
                lock_for(SettlementKey {
                    destination: storage.callback.destination.clone(),
                    object_path: storage.callback.object_path.clone(),
                    flow_id: context.flow_id.clone(),
                })
                .lock_owned()
                .await,
            )
        };
        Self {
            flow_id: context.flow_id.clone(),
            run_id: run_id.to_owned(),
            node_ids,
            _guard: guard,
        }
    }

    /// Settles every dedup node against the module's real Catalog-backed
    /// FlowState. The scope remains held until the caller drops `self`.
    pub(super) async fn settle(&self, catalog: FlowCatalogDocuments, success: bool) {
        if self.node_ids.is_empty() {
            return;
        }
        let namespace = format!("flow:{}", self.flow_id);
        let node_ids = self.node_ids.clone();
        let run_id = self.run_id.clone();
        match tokio::task::spawn_blocking(move || {
            let state = FlowStateDocuments::new(catalog, namespace);
            for node_id in node_ids {
                let result = dedup_settle::settle(&state, &node_id, Some(run_id.as_str()), success);
                tracing::debug!(run_id, node_id, ?result, "settled dedup node state");
            }
        })
        .await
        {
            Ok(()) => {}
            Err(error) => {
                tracing::error!(%error, run_id = %self.run_id, "dedup settlement worker failed")
            }
        }
    }
}

/// Decode and settle the graph snapshot owned by a lifecycle cleanup path.
/// Returning the scope lets the caller keep the same owner/flow lock held
/// through checkpoint deletion and terminal event persistence.
pub(super) async fn from_snapshot(
    storage: &tinyflows_bus::catalog::CatalogStorage,
    catalog: FlowCatalogDocuments,
    snapshot: Option<&Value>,
    run_id: &str,
    success: bool,
) -> Option<DedupSettlementScope> {
    let snapshot = snapshot?;
    let decoded = (|| {
        Some((
            tinyflows::migrate::deserialize_graph(snapshot.get("graph")?.clone()).ok()?,
            serde_json::from_value::<RunContext>(snapshot.get("context")?.clone()).ok()?,
        ))
    })();
    let Some((graph, context)) = decoded else {
        tracing::warn!(
            run_id,
            "run snapshot could not be decoded for dedup settlement"
        );
        return None;
    };
    let settlement = DedupSettlementScope::acquire(storage, &context, &graph, run_id).await;
    settlement.settle(catalog, success).await;
    Some(settlement)
}

#[cfg(test)]
#[path = "dedup_settlement_tests.rs"]
mod tests;
