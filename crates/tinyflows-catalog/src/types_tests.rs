use super::*;
use tinyflows::model::{Node, NodeKind};

fn sample_graph() -> WorkflowGraph {
    WorkflowGraph {
        nodes: vec![Node {
            id: "t".to_string(),
            kind: NodeKind::Trigger,
            type_version: 1,
            name: "Trigger".to_string(),
            config: serde_json::Value::Null,
            ports: Vec::new(),
            position: None,
        }],
        ..Default::default()
    }
}

#[test]
fn flow_round_trips_through_json() {
    let flow = Flow {
        id: "flow_1".to_string(),
        name: "demo".to_string(),
        enabled: true,
        graph: sample_graph(),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-01T00:00:00Z".to_string(),
        last_run_at: None,
        last_status: None,
        require_approval: false,
    };
    let json = serde_json::to_string(&flow).expect("serialize");
    let back: Flow = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back.id, flow.id);
    assert_eq!(back.graph, flow.graph);
    assert!(back.last_run_at.is_none());
    assert!(!back.require_approval);
}

#[test]
fn flow_require_approval_defaults_false_when_omitted_from_json() {
    // Legacy/serialized JSON authored before the field existed must still
    // deserialize (SQLite rows are migrated via `add_column_if_missing`,
    // but any bare JSON fixture should also default safely).
    let json = serde_json::json!({
        "id": "flow_1",
        "name": "demo",
        "enabled": true,
        "graph": sample_graph(),
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-01T00:00:00Z",
    });
    let flow: Flow = serde_json::from_value(json).expect("deserialize");
    assert!(!flow.require_approval);
}

#[test]
fn flow_run_round_trips_through_json() {
    let run = FlowRun {
        id: "flow:flow_1:run-uuid".to_string(),
        flow_id: "flow_1".to_string(),
        thread_id: "flow:flow_1:run-uuid".to_string(),
        status: "completed".to_string(),
        started_at: "2026-01-01T00:00:00Z".to_string(),
        finished_at: Some("2026-01-01T00:00:01Z".to_string()),
        steps: vec![FlowRunStep {
            node_id: "t".to_string(),
            output: serde_json::json!([{"json": {"hello": "world"}}]),
            port: None,
            ..Default::default()
        }],
        pending_approvals: Vec::new(),
        error: None,
        graph_hash: None,
    };
    let json = serde_json::to_string(&run).expect("serialize");
    let back: FlowRun = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back.id, run.id);
    assert_eq!(back.steps.len(), 1);
    assert_eq!(back.steps[0].node_id, "t");
    assert!(back.steps[0].port.is_none());
}

#[test]
fn flow_run_step_omits_port_when_none() {
    let step = FlowRunStep {
        node_id: "n".to_string(),
        output: serde_json::Value::Null,
        port: None,
        ..Default::default()
    };
    let v = serde_json::to_value(&step).unwrap();
    assert!(v.get("port").is_none());
}

#[test]
fn suggestion_status_token_round_trips() {
    for st in [
        SuggestionStatus::New,
        SuggestionStatus::Dismissed,
        SuggestionStatus::Built,
    ] {
        assert_eq!(SuggestionStatus::from_str_lossy(st.as_str()), st);
    }
    // Unknown tokens fall back to New rather than erroring.
    assert_eq!(
        SuggestionStatus::from_str_lossy("something_new"),
        SuggestionStatus::New
    );
    assert_eq!(SuggestionStatus::default(), SuggestionStatus::New);
}

#[test]
fn flow_suggestion_round_trips_through_json() {
    let s = FlowSuggestion {
        id: "sug_abc".to_string(),
        title: "Auto-file email receipts".to_string(),
        one_liner: "When a Gmail receipt arrives, add a row to your expenses sheet.".to_string(),
        rationale: "You forward receipts to yourself most weeks.".to_string(),
        trigger_hint: Some("app_event".to_string()),
        steps_outline: vec![
            "Watch Gmail for receipts".to_string(),
            "Extract amount + vendor".to_string(),
        ],
        suggested_connections: vec!["composio:gmail:conn_1".to_string()],
        suggested_slugs: vec!["GMAIL_NEW_GMAIL_MESSAGE".to_string()],
        build_prompt: "Build a workflow that…".to_string(),
        confidence: 0.82,
        status: SuggestionStatus::New,
        created_at: "2026-07-05T00:00:00Z".to_string(),
        source_run_id: Some("run-1".to_string()),
    };
    let json = serde_json::to_string(&s).expect("serialize");
    let back: FlowSuggestion = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, s);
}

#[test]
fn flow_suggestion_defaults_optional_fields() {
    // A minimal pitch (no trigger/steps/connections/slugs/status/run) must
    // deserialize with safe defaults.
    let json = serde_json::json!({
        "id": "sug_min",
        "title": "Daily digest",
        "one_liner": "Summarize your unread mail each morning.",
        "rationale": "You check mail first thing.",
        "build_prompt": "Build a scheduled digest…",
        "created_at": "2026-07-05T00:00:00Z",
    });
    let s: FlowSuggestion = serde_json::from_value(json).expect("deserialize");
    assert!(s.trigger_hint.is_none());
    assert!(s.steps_outline.is_empty());
    assert!(s.suggested_connections.is_empty());
    assert!(s.suggested_slugs.is_empty());
    assert_eq!(s.confidence, 0.0);
    assert_eq!(s.status, SuggestionStatus::New);
    assert!(s.source_run_id.is_none());
}
