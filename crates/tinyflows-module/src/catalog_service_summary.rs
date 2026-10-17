//! Catalog-list projection from persisted flows to the compact host summary.

use anyhow::Result;
use serde_json::Value;
use tinyflows_bus::catalog::WorkflowSummary;
use tinyflows_catalog::Flow;
use tinyflows_drivers::catalog::FlowCatalogDocuments;

pub(super) async fn summary(store: &FlowCatalogDocuments, flow: Flow) -> Result<WorkflowSummary> {
    let trigger_kind = serde_json::to_value(&flow.graph)?
        .get("nodes")
        .and_then(Value::as_array)
        .and_then(|nodes| {
            nodes
                .iter()
                .find(|node| node.get("kind").and_then(Value::as_str) == Some("trigger"))
        })
        .and_then(|node| node.get("config"))
        .and_then(|config| config.get("trigger_kind"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let revision = store.latest_revision_number(&flow.id).await?.unwrap_or(1);
    Ok(WorkflowSummary {
        id: flow.id,
        name: flow.name,
        revision,
        enabled: flow.enabled,
        require_approval: flow.require_approval,
        last_status: flow.last_status,
        trigger_kind,
        node_count: flow.graph.nodes.len(),
        updated_at: flow.updated_at,
    })
}
