use super::*;

#[tokio::test]
async fn explicit_catalog_migration_preserves_records_and_imports_legacy_state() {
    let (_server, host) = connections().await;
    let (storage, docs, fail_revision_put) =
        scope_with_test_options(&host, "migration", true, true).await;
    let created_ms = 1_772_600_767_250_i64;
    for collection in [
        "flows_definitions",
        "flows_revisions",
        "flows_kv",
        "flows_state",
    ] {
        docs.ensure_collection(&CollectionSpec::new(collection))
            .await
            .unwrap();
    }
    docs.put(
        "flows_definitions",
        "legacy-flow",
        json!({"name":"Legacy","graph_json":graph("manual", false).to_string(),
            "enabled":true,"created_at":"2026-03-04T05:06:07.250+00:00",
            "updated_at":"2026-03-04T05:06:07.250+00:00","require_approval":false,
            "incarnation":"legacy-inc"}),
        Precondition::None,
    )
    .await
    .unwrap();
    let revision_created_ms = 1_700_000_123_456_i64;
    docs.put_with(
        "flows_revisions",
        "legacy-revision",
        json!({"flow_id":"legacy-flow","name":"Older","graph_json":graph("manual", false).to_string(),
            "require_approval":false,"flow_incarnation":"legacy-inc"}),
        Precondition::None,
        WriteOptions::new().created_ms(revision_created_ms),
    )
    .await
    .unwrap();
    docs.put(
        "flows_kv",
        "legacy-kv",
        json!({"namespace":"legacy-flow","key":"k","value_json":"{\"n\":7}"}),
        Precondition::None,
    )
    .await
    .unwrap();
    docs.put(
        "flows_state",
        "legacy-state",
        json!({"key":"state-key","value":"state-value"}),
        Precondition::None,
    )
    .await
    .unwrap();

    let proxy = put_proxy(&host).unwrap();
    let first_attempt = proxy
        .call::<()>("MigrateStorage", &(storage.clone(),))
        .await;
    assert!(
        first_attempt.is_err(),
        "the injected mid-migration error is reported"
    );
    assert!(!fail_revision_put.load(Ordering::SeqCst));
    assert!(
        docs.get("flows_revisions", "legacy-revision")
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        docs.get("flows.revisions", "legacy-revision")
            .await
            .unwrap()
            .is_none()
    );

    let retry_args_a = (storage.clone(),);
    let retry_args_b = (storage.clone(),);
    let (retry_a, retry_b) = tokio::join!(
        proxy.call::<()>("MigrateStorage", &retry_args_a),
        proxy.call::<()>("MigrateStorage", &retry_args_b),
    );
    retry_a.unwrap();
    retry_b.unwrap();
    let migrated = docs
        .get("flows.definitions", "legacy-flow")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(migrated.doc["name"], "Legacy");
    assert_eq!(migrated.doc["created_ms"], created_ms);
    assert_eq!(migrated.meta.created_ms, created_ms);
    let migrated_revision = docs
        .get("flows.revisions", "legacy-revision")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(migrated_revision.doc["created_ms"], revision_created_ms);
    assert_eq!(migrated_revision.meta.created_ms, revision_created_ms);
    assert!(
        docs.get("flows_definitions", "legacy-flow")
            .await
            .unwrap()
            .is_none()
    );
    let catalog_state = Key::of(["flows", "kv:legacy-flow", "k"]).unwrap();
    let state = Key::of(["flows", "state", "state-key"]).unwrap();
    assert_eq!(
        docs.get("tsd.kv", catalog_state.as_str())
            .await
            .unwrap()
            .unwrap()
            .doc["value"],
        json!({"n":7})
    );
    assert_eq!(
        docs.get("tsd.kv", state.as_str())
            .await
            .unwrap()
            .unwrap()
            .doc["value"],
        "state-value"
    );
}

#[tokio::test]
async fn catalog_migration_refuses_missing_record_metadata_before_writing() {
    let (_server, host) = connections().await;
    let (storage, docs, _) = scope_with_test_options(&host, "missingmetadata", false, false).await;
    docs.ensure_collection(&CollectionSpec::new("flows_definitions"))
        .await
        .unwrap();
    docs.put(
        "flows_definitions",
        "legacy-flow",
        json!({"name":"Legacy","graph_json":graph("manual", false).to_string(),
            "enabled":true,"require_approval":false}),
        Precondition::None,
    )
    .await
    .unwrap();
    let proxy = put_proxy(&host).unwrap();

    assert!(
        proxy
            .call::<()>("MigrateStorage", &(storage,))
            .await
            .is_err()
    );
    assert!(
        docs.get("flows_definitions", "legacy-flow")
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        docs.get("flows.definitions", "legacy-flow")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        docs.get("tsd.migrations", "flows/flows.definitions/definitions_v2")
            .await
            .unwrap()
            .is_none()
    );
}
