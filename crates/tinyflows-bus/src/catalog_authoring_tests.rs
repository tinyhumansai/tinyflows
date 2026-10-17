use super::*;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::json;

fn wire_round_trip<T>(value: T)
where
    T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let encoded = serde_json::to_vec(&value).unwrap();
    let decoded: T = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(decoded, value);
}

#[test]
fn served_catalog_version_and_member_list_match_the_new_surface() {
    assert_eq!(CONTRACT_VERSION, 5);
    assert_eq!(
        METHODS,
        [
            "ListWorkflows",
            "GetWorkflow",
            "PutWorkflow",
            "DeleteWorkflow",
            "SetEnabled",
            "ListRevisions",
            "GetRevision",
            "CreateDraft",
            "GetDraft",
            "UpdateDraft",
            "ListDrafts",
            "DeleteDraft",
            "PromoteDraft",
            "UpsertSuggestions",
            "ListSuggestions",
            "SetSuggestionStatus",
            "GetRun",
            "ListRuns",
            "ListAllRuns",
            "PruneRuns",
            "NodeKinds",
            "ApplyGraphOps",
            "ApprovalManifest",
            "MigrateStorage",
        ]
    );
    assert_eq!(
        STORAGE_METHODS,
        [
            "DocCapabilities",
            "DocEnsure",
            "DocGet",
            "DocPut",
            "DocDelete",
            "DocQuery",
            "DocDeleteWhere",
        ]
    );
}

#[test]
fn approval_manifest_callback_contract_is_a_catalog_dependency() {
    assert_eq!(
        APPROVAL_MANIFEST_HOST_INTERFACE,
        "ai.tinyhumans.tinyflows.ApprovalManifestHost"
    );
    assert_eq!(CLASSIFY_MANIFEST_CANDIDATES, "ClassifyManifestCandidates");
}

#[test]
fn authoring_catalog_types_round_trip_and_match_literal_wire_shapes() {
    let ops = ApplyGraphOpsRequest {
        graph: json!({"nodes":[],"edges":[]}),
        ops: vec![json!({"op":"set_workflow_inputs","inputs":[]})],
    };
    assert_eq!(
        serde_json::to_value(&ops).unwrap(),
        json!({"graph":{"nodes":[],"edges":[]},"ops":[{"op":"set_workflow_inputs","inputs":[]}]})
    );
    wire_round_trip(ops);
    let error = ApplyGraphOpsResult {
        graph: None,
        error: Some(ApplyGraphOpsError {
            index: 2,
            op: "remove_node".into(),
            details: "no node with id missing".into(),
        }),
    };
    assert_eq!(
        serde_json::to_value(&error).unwrap(),
        json!({"graph":null,"error":{"index":2,"op":"remove_node","details":"no node with id missing"}})
    );
    wire_round_trip(error);

    let candidates = [
        ManifestCandidate::Http {
            node_id: "http".into(),
            url: "https://example.com".into(),
        },
        ManifestCandidate::Code {
            node_id: "code".into(),
        },
        ManifestCandidate::DynamicTool {
            node_id: "dynamic".into(),
        },
        ManifestCandidate::NativeTool {
            node_id: "native".into(),
            tool_name: "search".into(),
            args: json!({"query":"x"}),
        },
        ManifestCandidate::ConnectorTool {
            node_id: "connector".into(),
            slug: "mail.send".into(),
        },
        ManifestCandidate::Agent {
            node_id: "agent".into(),
        },
    ];
    for candidate in candidates {
        wire_round_trip(candidate);
    }
    let candidate = ManifestCandidate::NativeTool {
        node_id: "send".into(),
        tool_name: "web_search".into(),
        args: json!({"query":"x"}),
    };
    assert_eq!(
        serde_json::to_value(&candidate).unwrap(),
        json!({"kind":"native_tool","node_id":"send","tool_name":"web_search","args":{"query":"x"}})
    );
    wire_round_trip(ManifestClass {
        name: "Write".into(),
        blocked: false,
        skip: false,
    });
    wire_round_trip(ClassifyManifestCandidatesRequest {
        candidates: vec![candidate],
    });
    wire_round_trip(ApprovalManifestRequest {
        graph: json!({"nodes":[],"edges":[]}),
        native_prefix: "oh:".into(),
        host: CallbackAddress {
            destination: "test.ApprovalManifestHost".into(),
            object_path: "/test/alice/approval".into(),
        },
    });
}
