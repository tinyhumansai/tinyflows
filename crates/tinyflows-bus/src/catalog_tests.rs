use super::*;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::json;

fn wire_round_trip<T>(value: T)
where
    T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let bytes = serde_json::to_vec(&value).unwrap();
    let decoded: T = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(decoded, value);
}

fn storage() -> CatalogStorage {
    CatalogStorage {
        callback: CallbackAddress {
            destination: "test.CatalogStorage".into(),
            object_path: "/test/agent/alice/CatalogStorage".into(),
        },
    }
}

fn workflow() -> Workflow {
    Workflow {
        id: "flow-1".into(),
        name: "Example".into(),
        enabled: false,
        graph: json!({"nodes": [], "edges": []}),
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-02T00:00:00Z".into(),
        last_run_at: None,
        last_status: Some("completed".into()),
        require_approval: true,
    }
}

#[test]
fn every_catalog_contract_type_round_trips_on_the_wire() {
    let workflow = workflow();
    let storage = storage();
    wire_round_trip(DocEnsureRequest {
        spec: CatalogCollectionSpec {
            name: "flows.definitions".into(),
            indexes: vec![CatalogIndexSpec {
                name: "by_enabled".into(),
                fields: vec!["enabled".into(), "created_ms".into()],
                unique: false,
            }],
            ttl_field: None,
            search: Some(CatalogSearchSpec {
                fields: vec!["name".into()],
            }),
        },
    });
    wire_round_trip(DocGetRequest {
        collection: "flows.definitions".into(),
        id: "flow-1".into(),
    });
    wire_round_trip(DocPutRequest {
        collection: "flows.definitions".into(),
        id: "flow-1".into(),
        doc: json!({"name":"Example"}),
        precondition: CatalogDocumentPrecondition::Absent,
        schema: 2,
        expires_ms: Some(1_700_000_000_000),
        created_ms: Some(1_600_000_000_000),
    });
    wire_round_trip(DocDeleteRequest {
        collection: "flows.definitions".into(),
        id: "flow-1".into(),
        precondition: CatalogDocumentPrecondition::Version(1),
    });
    wire_round_trip(DocQueryRequest {
        collection: "flows.definitions".into(),
        query: CatalogQuery {
            filter: CatalogFilter::All,
            sort: vec![CatalogSort {
                field: "created_ms".into(),
                direction: CatalogSortDirection::Desc,
            }],
            limit: Some(25),
            cursor: Some(CatalogDocumentCursor("opaque-position".into())),
        },
    });
    wire_round_trip(DocDeleteWhereRequest {
        collection: "flows.runs".into(),
        filter: CatalogFilter::Eq {
            field: "flow_id".into(),
            value: json!("flow-1"),
        },
    });
    wire_round_trip(CatalogDocumentPrecondition::None);
    wire_round_trip(CatalogDocumentPrecondition::Version(8));
    wire_round_trip(CatalogVersionedDocument {
        id: "flow-1".into(),
        version: 7,
        doc: json!({"name":"Example"}),
        meta: CatalogDocumentMeta {
            schema: 2,
            created_ms: 1_600_000_000_000,
            updated_ms: 1_700_000_000_000,
            expires_ms: None,
        },
    });
    let legacy_read: CatalogVersionedDocument = serde_json::from_value(json!({
        "id":"old", "version":1, "doc":{}
    }))
    .unwrap();
    assert_eq!(legacy_read.meta, CatalogDocumentMeta::default());
    wire_round_trip(CatalogDocumentCursor("cursor-token".into()));
    for filter in [
        CatalogFilter::All,
        CatalogFilter::Eq {
            field: "f".into(),
            value: json!(1),
        },
        CatalogFilter::Ne {
            field: "f".into(),
            value: json!(1),
        },
        CatalogFilter::In {
            field: "f".into(),
            values: vec![json!(1), json!(2)],
        },
        CatalogFilter::Range {
            field: "f".into(),
            gt: Some(json!(1)),
            gte: None,
            lt: None,
            lte: Some(json!(4)),
        },
        CatalogFilter::Exists {
            field: "f".into(),
            exists: true,
        },
        CatalogFilter::And {
            filters: vec![CatalogFilter::All],
        },
        CatalogFilter::Or {
            filters: vec![CatalogFilter::All],
        },
        CatalogFilter::Not {
            filter: Box::new(CatalogFilter::All),
        },
    ] {
        wire_round_trip(filter);
    }
    wire_round_trip(CatalogSortDirection::Asc);
    wire_round_trip(CatalogSortDirection::Desc);
    wire_round_trip(CatalogSort {
        field: "_id".into(),
        direction: CatalogSortDirection::Asc,
    });
    wire_round_trip(CatalogDocumentPage {
        items: vec![CatalogVersionedDocument {
            id: "flow-1".into(),
            version: 1,
            doc: json!({}),
            meta: CatalogDocumentMeta::default(),
        }],
        next: None,
    });
    wire_round_trip(CatalogIndexSpec {
        name: "by_id".into(),
        fields: vec!["_id".into()],
        unique: true,
    });
    wire_round_trip(CatalogSearchSpec {
        fields: vec!["name".into()],
    });
    wire_round_trip(CatalogStorageFailure {
        kind: "conflict".into(),
        message: "version changed".into(),
    });
    wire_round_trip(CatalogStorageReply::Value {
        value: json!({"id":"flow-1"}),
    });
    wire_round_trip(CatalogStorageReply::Error {
        error: CatalogStorageFailure {
            kind: "not_found".into(),
            message: "missing".into(),
        },
    });
    wire_round_trip(storage.callback.clone());
    wire_round_trip(storage.clone());
    wire_round_trip(workflow.clone());
    wire_round_trip(WorkflowSummary {
        id: workflow.id.clone(),
        name: workflow.name.clone(),
        revision: 3,
        enabled: workflow.enabled,
        require_approval: workflow.require_approval,
        last_status: workflow.last_status.clone(),
        trigger_kind: Some("schedule".into()),
        node_count: 4,
        updated_at: workflow.updated_at.clone(),
    });
    wire_round_trip(WorkflowRevision {
        id: "revision-1".into(),
        flow_id: workflow.id.clone(),
        graph: workflow.graph.clone(),
        name: workflow.name.clone(),
        require_approval: workflow.require_approval,
        created_at: workflow.created_at.clone(),
    });
    wire_round_trip(PutWorkflowRequest::Create {
        storage: storage.clone(),
        name: "New".into(),
        graph: json!({"nodes": [], "edges": []}),
        require_approval: false,
    });
    wire_round_trip(PutWorkflowRequest::Update {
        storage: storage.clone(),
        id: workflow.id.clone(),
        name: workflow.name.clone(),
        graph: workflow.graph.clone(),
        require_approval: true,
        expected_version: Some(workflow.updated_at.clone()),
        force_disarm_automatic: true,
    });
    wire_round_trip(PutWorkflowRequest::Duplicate {
        storage: storage.clone(),
        id: workflow.id.clone(),
        name: "Example (copy)".into(),
    });
    wire_round_trip(PutWorkflowResult {
        workflow: workflow.clone(),
        born_disabled: true,
        approval_forced: true,
    });
    wire_round_trip(FlowUpdateError::VersionConflict {
        message: "changed".into(),
        current: Box::new(workflow.clone()),
    });
    wire_round_trip(FlowUpdateError::NotFound {
        message: "missing".into(),
    });
    wire_round_trip(FlowUpdateError::Store {
        message: "unavailable".into(),
    });
    wire_round_trip(PutWorkflowResponse::Ok(PutWorkflowResult {
        workflow: workflow.clone(),
        born_disabled: false,
        approval_forced: false,
    }));
    wire_round_trip(PutWorkflowResponse::Error(
        FlowUpdateError::VersionConflict {
            message: "changed".into(),
            current: Box::new(workflow.clone()),
        },
    ));
    wire_round_trip(ListWorkflowsResult {
        workflows: vec![],
        skipped_rows: 2,
    });
    wire_round_trip(DeleteWorkflowResult {
        id: workflow.id.clone(),
        removed: true,
    });
    wire_round_trip(SetEnabledRequest {
        storage: storage.clone(),
        id: workflow.id.clone(),
        enabled: true,
    });
    wire_round_trip(ListRevisionsRequest {
        storage: storage.clone(),
        flow_id: workflow.id.clone(),
        limit: 20,
    });
    wire_round_trip(GetRevisionRequest {
        storage,
        flow_id: workflow.id,
        revision_id: "revision-1".into(),
    });
}

#[test]
fn catalog_and_storage_member_json_shapes_are_pinned() {
    let storage = storage();
    assert_eq!(
        serde_json::to_value(&storage).unwrap(),
        json!({"callback":{"destination":"test.CatalogStorage","object_path":"/test/agent/alice/CatalogStorage"}})
    );
    assert_eq!(
        serde_json::to_value(DocGetRequest {
            collection: "flows.definitions".into(),
            id: "flow-1".into(),
        })
        .unwrap(),
        json!({"collection":"flows.definitions","id":"flow-1"})
    );
    assert_eq!(
        serde_json::to_value(DocEnsureRequest {
            spec: CatalogCollectionSpec {
                name: "flows.definitions".into(),
                indexes: vec![CatalogIndexSpec {
                    name: "by_enabled".into(),
                    fields: vec!["enabled".into(), "created_ms".into()],
                    unique: false,
                }],
                ttl_field: None,
                search: None,
            },
        })
        .unwrap(),
        json!({"spec":{"name":"flows.definitions","indexes":[{"name":"by_enabled","fields":["enabled","created_ms"],"unique":false}]}})
    );
    assert_eq!(
        serde_json::to_value(DocPutRequest {
            collection: "flows.definitions".into(),
            id: "flow-1".into(),
            doc: json!({"name":"Example"}),
            precondition: CatalogDocumentPrecondition::Version(7),
            schema: 0,
            expires_ms: None,
            created_ms: None,
        })
        .unwrap(),
        json!({"collection":"flows.definitions","id":"flow-1","doc":{"name":"Example"},"precondition":{"kind":"version","version":7}})
    );
    assert_eq!(
        serde_json::to_value(DocPutRequest {
            collection: "flows.definitions".into(),
            id: "flow-1".into(),
            doc: json!({"name":"Example"}),
            precondition: CatalogDocumentPrecondition::Absent,
            schema: 2,
            expires_ms: None,
            created_ms: None,
        })
        .unwrap(),
        json!({"collection":"flows.definitions","id":"flow-1","doc":{"name":"Example"},"precondition":{"kind":"absent"},"schema":2}),
        "a declared body version rides the put; an undeclared one is omitted"
    );
    // A request from before `schema` and `expires_ms` existed still decodes,
    // with both undeclared, and encodes back to the same shape.
    let old_shape = json!({"collection":"flows.definitions","id":"flow-1","doc":{"name":"Example"},"precondition":{"kind":"absent"}});
    let decoded: DocPutRequest = serde_json::from_value(old_shape.clone()).unwrap();
    assert_eq!((decoded.schema, decoded.expires_ms), (0, None));
    assert_eq!(serde_json::to_value(&decoded).unwrap(), old_shape);
    assert_eq!(
        serde_json::to_value(DocQueryRequest {
            collection: "flows.definitions".into(),
            query: CatalogQuery {
                filter: CatalogFilter::Eq {
                    field: "flow_id".into(),
                    value: json!("f1")
                },
                sort: vec![CatalogSort {
                    field: "created_ms".into(),
                    direction: CatalogSortDirection::Desc
                }],
                limit: Some(20),
                cursor: None,
            },
        })
        .unwrap(),
        json!({"collection":"flows.definitions","query":{"filter":{"op":"eq","field":"flow_id","value":"f1"},"sort":[{"field":"created_ms","direction":"desc"}],"limit":20}})
    );
    assert_eq!(
        serde_json::to_value(DocDeleteWhereRequest {
            collection: "flows.runs".into(),
            filter: CatalogFilter::Eq {
                field: "flow_id".into(),
                value: json!("f1")
            },
        })
        .unwrap(),
        json!({"collection":"flows.runs","filter":{"op":"eq","field":"flow_id","value":"f1"}})
    );
    assert_eq!(
        serde_json::to_value(CatalogStorageReply::Value {
            value: json!({"id":"flow-1","version":7,"doc":{"name":"Example"}}),
        })
        .unwrap(),
        json!({"value":{"id":"flow-1","version":7,"doc":{"name":"Example"}}})
    );
    assert_eq!(
        serde_json::to_value(CatalogStorageReply::Error {
            error: CatalogStorageFailure {
                kind: "conflict".into(),
                message: "version changed".into(),
            },
        })
        .unwrap(),
        json!({"error":{"kind":"conflict","message":"version changed"}})
    );
    assert_eq!(
        serde_json::to_value(PutWorkflowRequest::Update {
            storage,
            id: "flow-1".into(),
            name: "Updated".into(),
            graph: json!({"nodes":[],"edges":[]}),
            require_approval: true,
            expected_version: Some("2026-01-02T00:00:00Z".into()),
            force_disarm_automatic: true,
        })
        .unwrap(),
        json!({
            "operation":"update",
            "storage":{"callback":{"destination":"test.CatalogStorage","object_path":"/test/agent/alice/CatalogStorage"}},
            "id":"flow-1",
            "name":"Updated",
            "graph":{"nodes":[],"edges":[]},
            "require_approval":true,
            "expected_version":"2026-01-02T00:00:00Z",
            "force_disarm_automatic":true
        })
    );
    let current = workflow();
    assert_eq!(
        serde_json::to_value(PutWorkflowResponse::Error(
            FlowUpdateError::VersionConflict {
                message: "changed".into(),
                current: Box::new(current.clone()),
            },
        ))
        .unwrap(),
        json!({
            "status":"error",
            "value":{
                "code":"version_conflict",
                "message":"changed",
                "current":current
            }
        })
    );
    let summary = WorkflowSummary {
        id: current.id,
        name: current.name,
        revision: 2,
        enabled: false,
        require_approval: true,
        last_status: Some("completed".into()),
        trigger_kind: Some("schedule".into()),
        node_count: 4,
        updated_at: current.updated_at,
    };
    assert_eq!(
        serde_json::to_value(summary).unwrap(),
        json!({
            "id":"flow-1","name":"Example","revision":2,"enabled":false,
            "require_approval":true,"last_status":"completed","trigger_kind":"schedule",
            "node_count":4,"updated_at":"2026-01-02T00:00:00Z"
        })
    );
}
