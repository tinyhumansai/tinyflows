//! One suite every vault backend passes.
//!
//! Public for the same reason [`crate::ledger::conformance`] is: a host writing
//! a fourth backend runs the identical cases against it, so "it works on
//! sqlite" cannot quietly mean "it works only on sqlite".

use tinyflows::model::{Node, NodeKind, WorkflowGraph};
use tinyflows::store::types::{WorkflowDefaults, WorkflowRecord};

use super::Vault;

/// A record that a validating store would accept.
#[must_use]
pub fn record(id: &str) -> WorkflowRecord {
    WorkflowRecord {
        id: id.to_string(),
        name: id.to_string(),
        description: format!("does the {id} thing"),
        enabled: true,
        defaults: WorkflowDefaults::default(),
        graph: WorkflowGraph {
            schema_version: 1,
            id: Some(id.to_string()),
            name: id.to_string(),
            inputs: Vec::new(),
            agents: Vec::new(),
            nodes: vec![Node {
                id: "start".to_string(),
                kind: NodeKind::Trigger,
                type_version: 1,
                name: "manual".to_string(),
                config: serde_json::json!({ "trigger_kind": "manual" }),
                ports: Vec::new(),
                position: None,
            }],
            edges: Vec::new(),
        },
        source_path: None,
    }
}

/// Run every case against `vault`.
///
/// # Panics
/// On any conformance failure.
pub async fn run_all(vault: &dyn Vault) {
    an_empty_vault_loads_nothing_rather_than_erroring(vault).await;
    a_record_round_trips_whole(vault).await;
    putting_the_same_id_twice_replaces_rather_than_duplicating(vault).await;
    removing_what_is_not_there_is_not_an_error(vault).await;
    a_removed_record_stops_loading(vault).await;
}

async fn an_empty_vault_loads_nothing_rather_than_erroring(vault: &dyn Vault) {
    assert!(
        vault
            .load()
            .await
            .expect("load")
            .iter()
            .all(|r| r.id != "never-written"),
        "nothing was written under that id"
    );
}

async fn a_record_round_trips_whole(vault: &dyn Vault) {
    let mut want = record("wf-round");
    want.description = "carries prose a planner reads".to_string();
    want.enabled = false;
    vault.put(&want).await.expect("put");

    let got = vault
        .load()
        .await
        .expect("load")
        .into_iter()
        .find(|r| r.id == "wf-round")
        .expect("stored");
    assert_eq!(got.description, want.description);
    assert!(!got.enabled, "an operator's off switch survives the trip");
    assert_eq!(got.graph.nodes.len(), 1, "the graph is the point");
    assert_eq!(got.graph.nodes[0].kind, NodeKind::Trigger);
}

async fn putting_the_same_id_twice_replaces_rather_than_duplicating(vault: &dyn Vault) {
    // Two episodes arriving at the same procedure write the same content-derived
    // id. That must converge, not accumulate.
    vault.put(&record("wf-twice")).await.expect("put");
    vault.put(&record("wf-twice")).await.expect("put");
    assert_eq!(
        vault
            .load()
            .await
            .expect("load")
            .iter()
            .filter(|r| r.id == "wf-twice")
            .count(),
        1
    );
}

async fn removing_what_is_not_there_is_not_an_error(vault: &dyn Vault) {
    vault.remove("wf-absent").await.expect("remove");
}

async fn a_removed_record_stops_loading(vault: &dyn Vault) {
    vault.put(&record("wf-gone")).await.expect("put");
    vault.remove("wf-gone").await.expect("remove");
    assert!(
        !vault
            .load()
            .await
            .expect("load")
            .iter()
            .any(|r| r.id == "wf-gone"),
        "a delete that only hides the row is a delete nobody can trust"
    );
}

/// Run every tenant-isolation case. Three handles onto one store.
///
/// # Panics
/// On any isolation failure — each is one tenant's procedure appearing in
/// another's catalogue.
pub async fn run_tenants(global: &dyn Vault, a: &dyn Vault, b: &dyn Vault) {
    assert_eq!(global.scope(), None);
    assert_ne!(a.scope(), b.scope());

    a.put(&record("wf-mine")).await.expect("put");
    assert!(
        a.load()
            .await
            .expect("load")
            .iter()
            .any(|r| r.id == "wf-mine"),
        "a tenant sees its own"
    );
    assert!(
        !b.load()
            .await
            .expect("load")
            .iter()
            .any(|r| r.id == "wf-mine"),
        "tenant {:?} can read tenant {:?}'s workflow",
        b.scope(),
        a.scope()
    );

    global.put(&record("wf-shared")).await.expect("put");
    for tenant in [a, b] {
        assert!(
            tenant
                .load()
                .await
                .expect("load")
                .iter()
                .any(|r| r.id == "wf-shared"),
            "tenant {:?} cannot see a global workflow",
            tenant.scope()
        );
    }
}
