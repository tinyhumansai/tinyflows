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
async fn edit_workflow_falls_back_to_draft_name_when_no_name_arg() {
    let host = Arc::new(crate::tools::fakes::MemHost::default());
    host.put_draft("d1", None, "Drafty", nameless_graph());
    let tool = EditWorkflowTool::new(host.clone(), host.clone(), host.clone());

    let result = tool
        .execute(json!({
            "draft_id": "d1",
            "ops": [{"op": "set_node_position", "id": "start", "position": {"x": 1.0, "y": 2.0}}]
        }))
        .await
        .unwrap();
    assert!(!result.is_error, "edit failed: {}", result.text());

    let draft = host.drafts.lock().unwrap().get("d1").unwrap().clone();
    assert_eq!(
        draft.graph.get("name").and_then(Value::as_str),
        Some("Drafty"),
        "draft graph did not fall back to the draft's own name"
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
