use super::*;

#[test]
fn interfaces_share_one_bus_name_with_distinct_paths() {
    let paths = [
        OBJECT_PATH,
        runs::OBJECT_PATH,
        catalog::OBJECT_PATH,
        schedule::OBJECT_PATH,
    ];
    for (i, a) in paths.iter().enumerate() {
        for b in &paths[i + 1..] {
            assert_ne!(a, b);
        }
    }
    assert!(runs::INTERFACE.starts_with("ai.tinyhumans.tinyflows."));
    assert!(catalog::INTERFACE.starts_with("ai.tinyhumans.tinyflows."));
}

#[test]
fn method_constants_match_advertised_members() {
    assert_eq!(portability::CONTRACT_VERSION, 2);
    assert_eq!(
        portability::METHODS,
        &[
            "PrepareHttpRequest",
            "AppEventBinding",
            "PinnedInputs",
            "DescribeTrigger"
        ]
    );
    assert_eq!(copilot::CONTRACT_VERSION, 1);
    assert_eq!(
        runs::METHODS,
        &[
            runs::methods::START_RUN,
            runs::methods::RESUME_RUN,
            runs::methods::EXPIRE_PARKED,
            runs::methods::BOOT_SWEEP,
            runs::methods::CANCEL_RUN,
            runs::methods::POLL_RUN,
            runs::methods::SANDBOX_RUN
        ]
    );
    assert_eq!(
        copilot::METHODS,
        [
            "ToolCatalog",
            "InvokeTool",
            "RenderBuilderPrompt",
            "GetPrompt",
            "ListResources",
            "GetResource",
            "BuildTrailOffFallback",
            "CombineTrailOffFallback",
            "IsBackendOrInfrastructureFailure",
            "BackendRepairMessage",
            "TextLooksLikeQuestion",
        ]
    );
}

#[test]
fn authoring_contract_version_and_methods_pin_the_new_requests() {
    assert_eq!(CONTRACT_VERSION, 3);
    assert_eq!(
        METHODS,
        [
            "ValidateGraph",
            "BindingFailures",
            "AuthoringFailures",
            "UnresolvableToolArgs",
            "BuilderFailures",
            "ImportGraph",
            "WiringWarnings",
        ]
    );
    assert_eq!(methods::VALIDATE_GRAPH, "ValidateGraph");
    assert_eq!(methods::IMPORT_GRAPH, "ImportGraph");
    let request = ValidateGraphRequest {
        graph: serde_json::json!({"nodes":[],"edges":[]}),
        host: None,
    };
    assert_eq!(
        serde_json::to_value(&request).unwrap(),
        serde_json::json!({"graph":{"nodes":[],"edges":[]},"host":null})
    );
    let decoded: ValidateGraphRequest =
        serde_json::from_value(serde_json::to_value(request).unwrap()).unwrap();
    assert_eq!(decoded.host, None);
    let import = ImportGraphRequest {
        graph: serde_json::json!({"nodes":[],"edges":[]}),
        format: Some("auto".into()),
    };
    assert_eq!(
        serde_json::to_value(&import).unwrap(),
        serde_json::json!({"graph":{"nodes":[],"edges":[]},"format":"auto"})
    );
    let imported: ImportGraphRequest =
        serde_json::from_value(serde_json::to_value(import).unwrap()).unwrap();
    assert_eq!(imported.format.as_deref(), Some("auto"));
    wire_import_result_round_trip();
}

#[test]
fn wiring_contract_wire_shapes_round_trip() {
    let contract = ToolContract {
        output_fields: vec!["issues".into()],
        output_schema: Some(serde_json::json!({"type":"object"})),
        primary_array_path: Some("data.issues".into()),
        envelope_prefix: Some("payload".into()),
        metadata_fields: vec!["request_id".into()],
    };
    let contract_value = serde_json::to_value(&contract).unwrap();
    assert_eq!(
        contract_value,
        serde_json::json!({
            "output_fields":["issues"],
            "output_schema":{"type":"object"},
            "primary_array_path":"data.issues",
            "envelope_prefix":"payload",
            "metadata_fields":["request_id"]
        })
    );
    assert_eq!(
        serde_json::from_value::<ToolContract>(contract_value).unwrap(),
        contract
    );
    let request = WiringWarningsRequest {
        graph: serde_json::json!({"nodes":[],"edges":[]}),
        host: AuthoringHost {
            bus_name: "test.Contracts".into(),
            object_path: "/contracts".into(),
        },
    };
    let encoded = serde_json::to_value(&request).unwrap();
    assert_eq!(
        encoded,
        serde_json::json!({
            "graph":{"nodes":[],"edges":[]},
            "host":{"bus_name":"test.Contracts","object_path":"/contracts"}
        })
    );
    assert_eq!(
        serde_json::from_value::<WiringWarningsRequest>(encoded).unwrap(),
        request
    );
    assert_eq!(methods::WIRING_WARNINGS, "WiringWarnings");
    assert_eq!(host::LOOKUP_TOOL_CONTRACT, "LookupToolContract");
}

fn wire_import_result_round_trip() {
    let value = ImportGraphResult {
        graph: Some(serde_json::json!({"nodes":[],"edges":[]})),
        warnings: vec!["one approximation".into()],
        errors: vec![ValidationError {
            code: "missing_trigger".into(),
            message: "missing trigger".into(),
            node_id: None,
            field: None,
        }],
    };
    assert_eq!(
        serde_json::to_value(&value).unwrap(),
        serde_json::json!({
            "graph":{"nodes":[],"edges":[]},
            "warnings":["one approximation"],
            "errors":[{"code":"missing_trigger","message":"missing trigger","node_id":null,"field":null}]
        })
    );
    let decoded: ImportGraphResult =
        serde_json::from_value(serde_json::to_value(&value).unwrap()).unwrap();
    assert_eq!(decoded, value);
}

#[test]
fn run_event_round_trips() {
    let ev = runs::RunEvent {
        seq: 3,
        kind: "step".into(),
        data: serde_json::json!({"a": 1}),
        graph_run_id: None,
    };
    let back: runs::RunEvent = serde_json::from_str(&serde_json::to_string(&ev).unwrap()).unwrap();
    assert_eq!(ev, back);
}
