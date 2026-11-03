//! Regression tests for tinyhumansai/openhuman#8453: `edit_workflow` must stamp
//! the resolved workflow name into `edited.name` before writing the draft, so
//! the durable draft graph (and anything saved from it) carries the name the
//! agent actually chose — not the base graph's name.

use super::*;
use crate::tools::SaveWorkflowTool;
use serde_json::json;
use tinytools::Tool;

fn nameless_graph() -> serde_json::Value {
    json!({
        "name": "",
        "nodes": [{"id": "start", "name": "Start", "kind": "trigger"}],
        "edges": []
    })
}

fn named_graph(name: &str) -> serde_json::Value {
    json!({
        "name": name,
        "nodes": [{"id": "start", "name": "Start", "kind": "trigger"}],
        "edges": []
    })
}

fn nudge_ops() -> serde_json::Value {
    json!([{"op": "set_node_position", "id": "start", "position": {"x": 1.0, "y": 2.0}}])
}

/// The proposal payload's resolved workflow name.
fn proposal_name(text: &str) -> Option<String> {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|p| p.get("name").and_then(Value::as_str).map(str::to_string))
}

/// The proposal payload's draft handle.
fn proposal_draft_id(text: &str) -> String {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|p| {
            p.get("draft_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .expect("proposal payload carries draft_id")
}

fn rename_args(draft_id: &str) -> serde_json::Value {
    json!({
        "draft_id": draft_id,
        "name": "Renamed Flow",
        "ops": [{"op": "set_node_position", "id": "start", "position": {"x": 1.0, "y": 2.0}}]
    })
}

#[tokio::test]
async fn edit_workflow_writes_resolved_name_into_draft_graph() {
    let host = Arc::new(crate::tools::fakes::MemHost::default());
    host.put_draft("d1", None, "Draft name", nameless_graph());
    let tool = EditWorkflowTool::new(host.clone(), host.clone(), host.clone());

    let result = tool.execute(rename_args("d1")).await.unwrap();
    assert!(!result.is_error, "edit failed: {}", result.text());

    let draft = host.drafts.lock().unwrap().get("d1").unwrap().clone();
    assert_eq!(
        draft.graph.get("name").and_then(Value::as_str),
        Some("Renamed Flow"),
        "draft graph kept the base graph's name instead of the resolved edit name"
    );
}

#[tokio::test]
async fn edit_workflow_falls_back_to_graph_name_when_no_name_arg() {
    // The base GRAPH's name is the fallback — not the draft's metadata name,
    // which can go stale (e.g. a save_workflow without a name arg advances the
    // graph but not the record).
    let host = Arc::new(crate::tools::fakes::MemHost::default());
    host.put_draft("d1", None, "Drafty", named_graph("Graphy"));
    let tool = EditWorkflowTool::new(host.clone(), host.clone(), host.clone());

    let result = tool
        .execute(json!({
            "draft_id": "d1",
            "ops": nudge_ops(),
        }))
        .await
        .unwrap();
    assert!(!result.is_error, "edit failed: {}", result.text());

    let draft = host.drafts.lock().unwrap().get("d1").unwrap().clone();
    assert_eq!(
        draft.graph.get("name").and_then(Value::as_str),
        Some("Graphy"),
        "draft graph did not fall back to the base graph's name"
    );
}

#[tokio::test]
async fn edit_workflow_uses_untitled_when_graph_name_empty() {
    // A stale/empty metadata name must not leak into the graph: with no name
    // arg and an empty graph name, the fallback is "Untitled workflow" even
    // when the draft record itself is named.
    let host = Arc::new(crate::tools::fakes::MemHost::default());
    host.put_draft("d1", None, "Drafty", nameless_graph());
    let tool = EditWorkflowTool::new(host.clone(), host.clone(), host.clone());

    let result = tool
        .execute(json!({
            "draft_id": "d1",
            "ops": nudge_ops(),
        }))
        .await
        .unwrap();
    assert!(!result.is_error, "edit failed: {}", result.text());

    let draft = host.drafts.lock().unwrap().get("d1").unwrap().clone();
    assert_eq!(
        draft.graph.get("name").and_then(Value::as_str),
        Some("Untitled workflow"),
        "draft graph used the stale draft metadata name instead of the Untitled fallback"
    );
}

#[tokio::test]
async fn edit_then_save_persists_resolved_name_in_flow_graph() {
    let host = Arc::new(crate::tools::fakes::MemHost::default());
    host.put_flow("f1", "Old Name", true, nameless_graph());
    host.put_draft("d1", Some("f1"), "Draft name", nameless_graph());
    let edit = EditWorkflowTool::new(host.clone(), host.clone(), host.clone());
    let save = SaveWorkflowTool::new(host.clone(), host.clone(), host.clone());

    let edited = edit.execute(rename_args("d1")).await.unwrap();
    assert!(!edited.is_error, "edit failed: {}", edited.text());
    let saved = save
        .execute(json!({"flow_id": "f1", "draft_id": "d1"}))
        .await
        .unwrap();
    assert!(!saved.is_error, "save failed: {}", saved.text());

    let flow = host.flows.lock().unwrap().get("f1").unwrap().clone();
    assert_eq!(
        flow.graph.name, "Renamed Flow",
        "saved flow graph kept the empty base name instead of the resolved edit name"
    );
}

#[tokio::test]
async fn flow_edit_save_edit_save_keeps_graph_name_over_stale_flow_name() {
    // Full staleness chain: a save_workflow WITHOUT a name arg advances the
    // flow's graph but leaves its top-level name behind. A later flow-based
    // edit must resolve the graph's (current) name — not the stale record
    // name — and saving again must not clobber the graph name back.
    let host = Arc::new(crate::tools::fakes::MemHost::default());
    host.put_flow("f1", "Original Name", true, named_graph("Original Name"));
    let edit = EditWorkflowTool::new(host.clone(), host.clone(), host.clone());
    let save = SaveWorkflowTool::new(host.clone(), host.clone(), host.clone());

    // 1. Rename through a flow-based edit, then save without a name arg.
    let first = edit
        .execute(json!({"flow_id": "f1", "name": "Renamed", "ops": nudge_ops()}))
        .await
        .unwrap();
    assert!(!first.is_error, "edit failed: {}", first.text());
    let draft1 = proposal_draft_id(&first.text().to_string());
    let saved1 = save
        .execute(json!({"flow_id": "f1", "draft_id": draft1}))
        .await
        .unwrap();
    assert!(!saved1.is_error, "save failed: {}", saved1.text());
    {
        let flow = host.flows.lock().unwrap().get("f1").unwrap().clone();
        assert_eq!(flow.name, "Original Name");
        assert_eq!(flow.graph.name, "Renamed");
    }

    // 2. A later flow-based edit with no name arg resolves the graph name.
    let second = edit
        .execute(json!({"flow_id": "f1", "ops": nudge_ops()}))
        .await
        .unwrap();
    assert!(!second.is_error, "edit failed: {}", second.text());
    assert_eq!(
        proposal_name(&second.text().to_string()).as_deref(),
        Some("Renamed"),
        "flow-based edit fell back to the stale flow name instead of the graph name"
    );
    let draft2 = proposal_draft_id(&second.text().to_string());

    // 3. Saving again keeps the current graph name.
    let saved2 = save
        .execute(json!({"flow_id": "f1", "draft_id": draft2}))
        .await
        .unwrap();
    assert!(!saved2.is_error, "save failed: {}", saved2.text());
    let flow = host.flows.lock().unwrap().get("f1").unwrap().clone();
    assert_eq!(
        flow.graph.name, "Renamed",
        "save_workflow overwrote the graph name with the stale flow name"
    );
}

#[tokio::test]
async fn draft_edit_save_edit_save_keeps_graph_name_over_stale_draft_name() {
    // Same staleness chain through a draft: the draft metadata carries the
    // stale record name (as drafts.create seeds it from flow.name) while its
    // graph carries the current name. Edit -> save -> edit -> save must keep
    // the graph's name throughout.
    let host = Arc::new(crate::tools::fakes::MemHost::default());
    host.put_flow("f1", "Stale Name", true, named_graph("Current Name"));
    host.put_draft("d1", Some("f1"), "Stale Name", named_graph("Current Name"));
    let edit = EditWorkflowTool::new(host.clone(), host.clone(), host.clone());
    let save = SaveWorkflowTool::new(host.clone(), host.clone(), host.clone());

    let first = edit
        .execute(json!({"draft_id": "d1", "ops": nudge_ops()}))
        .await
        .unwrap();
    assert!(!first.is_error, "edit failed: {}", first.text());
    assert_eq!(
        proposal_name(&first.text().to_string()).as_deref(),
        Some("Current Name"),
        "draft-based edit fell back to the stale draft name instead of the graph name"
    );

    let saved1 = save
        .execute(json!({"flow_id": "f1", "draft_id": "d1"}))
        .await
        .unwrap();
    assert!(!saved1.is_error, "save failed: {}", saved1.text());

    let second = edit
        .execute(json!({"draft_id": "d1", "ops": nudge_ops()}))
        .await
        .unwrap();
    assert!(!second.is_error, "edit failed: {}", second.text());
    assert_eq!(
        proposal_name(&second.text().to_string()).as_deref(),
        Some("Current Name"),
        "second draft-based edit lost the graph name"
    );

    let saved2 = save
        .execute(json!({"flow_id": "f1", "draft_id": "d1"}))
        .await
        .unwrap();
    assert!(!saved2.is_error, "save failed: {}", saved2.text());
    let flow = host.flows.lock().unwrap().get("f1").unwrap().clone();
    assert_eq!(
        flow.graph.name, "Current Name",
        "save_workflow overwrote the graph name with the stale draft name"
    );
}
