use super::*;
use crate::catalog;

fn pin<T>(value: T, expected: serde_json::Value)
where
    T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    assert_eq!(serde_json::to_value(&value).unwrap(), expected);
    assert_eq!(serde_json::from_value::<T>(expected).unwrap(), value);
}

fn storage() -> CatalogStorage {
    CatalogStorage {
        callback: catalog::CallbackAddress {
            destination: "host.docs".into(),
            object_path: "/owner/a".into(),
        },
    }
}

#[test]
fn run_start_contract_roundtrips_and_matches_literal_fixture() {
    pin(
        StartRun {
            run_id: "opaque/run:1".into(),
            context: RunContext {
                flow_id: "flow-1".into(),
                origin: Some(serde_json::json!({"schedule":"s-2"})),
            },
            caps_host: CapsHost {
                callback: CallbackAddress {
                    destination: "host.caps".into(),
                    object_path: "/run/opaque".into(),
                },
            },
            storage: storage(),
            graph: serde_json::json!({"nodes": [], "edges": []}),
            trigger_kind: FlowRunTrigger::Schedule,
            trigger_data: serde_json::json!({"task":"nightly"}),
            inputs: serde_json::Map::from_iter([("limit".into(), serde_json::json!(3))]),
            timeout_ms: 600_000,
        },
        serde_json::json!({
            "run_id":"opaque/run:1",
            "context":{"flow_id":"flow-1","origin":{"schedule":"s-2"}},
            "caps_host":{"callback":{"destination":"host.caps","object_path":"/run/opaque"}},
            "storage":{"callback":{"destination":"host.docs","object_path":"/owner/a"}},
            "graph":{"nodes":[],"edges":[]},
            "trigger_kind":"schedule",
            "trigger_data":{"task":"nightly"},
            "inputs":{"limit":3},
            "timeout_ms":600000
        }),
    );
    pin(
        StartRunResponse {
            run_id: "opaque/run:1".into(),
            already_running: true,
        },
        serde_json::json!({"run_id":"opaque/run:1","already_running":true}),
    );
    pin(
        RunContext {
            flow_id: "flow-2".into(),
            origin: None,
        },
        serde_json::json!({"flow_id":"flow-2","origin":null}),
    );
}

#[test]
fn run_control_and_poll_contracts_match_literal_fixtures() {
    pin(
        RunControlRequest {
            storage: storage(),
            run_id: "run-1".into(),
            cursor: 4,
        },
        serde_json::json!({"storage":{"callback":{"destination":"host.docs","object_path":"/owner/a"}},"run_id":"run-1","cursor":4}),
    );
    pin(
        CancelRunResponse { signalled: true },
        serde_json::json!({"signalled":true}),
    );
    pin(
        RunEvent {
            seq: 5,
            kind: "graph_observation".into(),
            data: serde_json::json!({"event":"node.started"}),
            graph_run_id: Some("graph-run-9".into()),
        },
        serde_json::json!({"seq":5,"kind":"graph_observation","data":{"event":"node.started"},"graph_run_id":"graph-run-9"}),
    );
    pin(
        RunEvent {
            seq: 6,
            kind: "step".into(),
            data: serde_json::json!({"node_id":"send"}),
            graph_run_id: None,
        },
        serde_json::json!({"seq":6,"kind":"step","data":{"node_id":"send"}}),
    );
    pin(
        PollRun {
            events: vec![],
            next_cursor: 4,
            done: false,
        },
        serde_json::json!({"events":[],"next_cursor":4,"done":false}),
    );
    for (trigger, fixture) in [
        (FlowRunTrigger::Rpc, "rpc"),
        (FlowRunTrigger::Schedule, "schedule"),
        (FlowRunTrigger::AppEvent, "app_event"),
        (FlowRunTrigger::Resume, "resume"),
    ] {
        pin(trigger, serde_json::json!(fixture));
    }
    pin(
        ResumeRun {
            run_id: "run-1".into(),
            flow_id: "flow-1".into(),
            storage: storage(),
            caps_host: CapsHost {
                callback: CallbackAddress {
                    destination: "host.caps".into(),
                    object_path: "/run/1".into(),
                },
            },
            approvals: vec!["gate".into()],
            rejections: vec!["decline".into()],
            timeout_ms: 60_000,
        },
        serde_json::json!({
            "run_id":"run-1",
            "flow_id":"flow-1",
            "storage":{"callback":{"destination":"host.docs","object_path":"/owner/a"}},
            "caps_host":{"callback":{"destination":"host.caps","object_path":"/run/1"}},
            "approvals":["gate"],
            "rejections":["decline"],
            "timeout_ms":60000
        }),
    );
    pin(
        ResumeRunResponse {
            run_id: "run-1".into(),
        },
        serde_json::json!({"run_id":"run-1"}),
    );
    pin(
        ExpireParked { storage: storage() },
        serde_json::json!({"storage":{"callback":{"destination":"host.docs","object_path":"/owner/a"}}}),
    );
    pin(
        ExpireParkedResponse {
            expired: vec![("run-1".into(), "flow-1".into())],
        },
        serde_json::json!({"expired":[["run-1","flow-1"]]}),
    );
    pin(
        BootSweep {
            storage: storage(),
            shared_backend: false,
        },
        serde_json::json!({"storage":{"callback":{"destination":"host.docs","object_path":"/owner/a"}},"shared_backend":false}),
    );
    pin(
        BootSweepResponse {
            interrupted: vec!["run-1".into()],
        },
        serde_json::json!({"interrupted":["run-1"]}),
    );
    pin(
        CallbackAddress {
            destination: "host.caps".into(),
            object_path: "/run/1".into(),
        },
        serde_json::json!({"destination":"host.caps","object_path":"/run/1"}),
    );
    pin(
        CapsHost {
            callback: CallbackAddress {
                destination: "host.caps".into(),
                object_path: "/run/1".into(),
            },
        },
        serde_json::json!({"callback":{"destination":"host.caps","object_path":"/run/1"}}),
    );
    pin(
        SandboxRun {
            graph: serde_json::json!({"nodes":[],"edges":[]}),
            input: serde_json::json!({}),
            tool_invoker: CallbackAddress {
                destination: "host.preflight".into(),
                object_path: "/sandbox/1".into(),
            },
            permissions_manifest: vec![serde_json::json!({"node_id":"send","risk":"write"})],
        },
        serde_json::json!({
            "graph":{"nodes":[],"edges":[]},
            "input":{},
            "tool_invoker":{"destination":"host.preflight","object_path":"/sandbox/1"},
            "permissions_manifest":[{"node_id":"send","risk":"write"}]
        }),
    );
    pin(
        SandboxRunResponse {
            result: serde_json::json!({"sandbox":true,"ok":true}),
        },
        serde_json::json!({"result":{"sandbox":true,"ok":true}}),
    );
    assert_eq!(CONTRACT_VERSION, 5);
    assert_eq!(
        METHODS,
        [
            "StartRun",
            "ResumeRun",
            "ExpireParked",
            "BootSweep",
            "CancelRun",
            "PollRun",
            "SandboxRun"
        ]
    );
    assert_eq!(CAPS_CONTRACT_VERSION, 1);
    assert_eq!(CAPS_METHODS.len(), 19);
}

#[test]
fn run_finished_payload_has_authoritative_success_and_optional_details() {
    pin(
        RunFinished {
            status: "completed_with_warnings".into(),
            successful: true,
            error: None,
            graph_run_id: Some("graph-run-1".into()),
        },
        serde_json::json!({
            "status":"completed_with_warnings",
            "successful":true,
            "graph_run_id":"graph-run-1"
        }),
    );
    pin(
        RunFinished {
            status: "cancelled".into(),
            successful: false,
            error: Some("cancelled by host".into()),
            graph_run_id: None,
        },
        serde_json::json!({
            "status":"cancelled",
            "successful":false,
            "error":"cancelled by host"
        }),
    );
    let legacy = serde_json::json!({"status":"completed"});
    let decoded: RunFinished = serde_json::from_value(legacy).unwrap();
    assert!(!decoded.successful, "missing legacy outcome fails closed");
}
