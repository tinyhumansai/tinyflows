//! Exercises portable helper calls through the actual loaded module ABI.

use serde_json::{Value, json};
use tinyflows_bus::{BUS_NAME, OBJECT_PATH, portability};

pub async fn verify_surface(connection: &tinybus::Connection) {
    let proxy = connection
        .proxy(BUS_NAME, OBJECT_PATH, portability::INTERFACE)
        .unwrap();
    let prepared: Value = proxy
        .call(
            "PrepareHttpRequest",
            (portability::PrepareHttpRequest {
                request: json!({"headers":{"authorization":"inline"}}),
                credential: portability::HttpCredentialTemplate::Basic {
                    username: "alice".into(),
                    secret: "secret".into(),
                },
            },),
        )
        .await
        .unwrap();
    assert_eq!(
        prepared["headers"]["Authorization"],
        "Basic YWxpY2U6c2VjcmV0"
    );
    assert!(prepared["headers"].get("authorization").is_none());

    for request in [json!(null), json!({"headers":"not-an-object"})] {
        let error = proxy
            .call::<Value>(
                "PrepareHttpRequest",
                (portability::PrepareHttpRequest {
                    request,
                    credential: portability::HttpCredentialTemplate::Bearer {
                        secret: "module-secret".into(),
                    },
                },),
            )
            .await
            .expect_err("malformed HTTP input must be rejected by the module");
        assert!(!error.to_string().contains("module-secret"));
    }

    for (kind, schedule) in [
        ("cron", json!({"kind":"cron","expr":"0 9 * * 1","tz":"UTC"})),
        ("at", json!({"kind":"at","at":"2027-01-01T00:00:00Z"})),
        ("every", json!({"kind":"every","every_ms":60000})),
    ] {
        let expected_schedule = serde_json::to_value(
            serde_json::from_value::<tinyflows_bus::schedule::Schedule>(schedule.clone()).unwrap(),
        )
        .unwrap();
        let graph = json!({"name":"schedule","nodes":[{"id":"t","name":"Trigger","kind":"trigger","config":{
            "trigger_kind":"schedule","schedule":schedule
        }}],"edges":[]});
        let details: portability::TriggerDetails = proxy
            .call("DescribeTrigger", (portability::DescribeTrigger { graph },))
            .await
            .expect("compiled module should describe each typed schedule variant");
        assert_eq!(details.kind.as_deref(), Some("schedule"));
        assert_eq!(
            serde_json::to_value(details.schedule).unwrap(),
            expected_schedule,
            "schedule variant {kind} should remain typed and intact"
        );
    }

    for kind in ["manual", "app_event", "webhook", "form", "chat_message"] {
        let graph = json!({"nodes":[{"id":"t","name":"Trigger","kind":"trigger","config":{"trigger_kind":kind}}],"edges":[]});
        let details: portability::TriggerDetails = proxy
            .call("DescribeTrigger", (portability::DescribeTrigger { graph },))
            .await
            .expect("compiled module should describe recognized trigger kinds");
        assert_eq!(details.kind.as_deref(), Some(kind));
        assert!(details.schedule.is_none());
    }

    for graph in [
        json!({"nodes":[],"edges":[]}),
        json!({"nodes":[
            {"id":"t1","name":"T1","kind":"trigger","config":{"trigger_kind":"schedule","schedule":{"kind":"at","at":"2027-01-01T00:00:00Z"}}},
            {"id":"t2","name":"T2","kind":"trigger","config":{"trigger_kind":"manual"}}
        ],"edges":[]}),
    ] {
        let details: portability::TriggerDetails = proxy
            .call("DescribeTrigger", (portability::DescribeTrigger { graph },))
            .await
            .expect("missing or ambiguous trigger metadata should be a safe absence");
        assert_eq!(
            details,
            portability::TriggerDetails {
                kind: None,
                schedule: None
            }
        );
    }

    let missing_schedule = json!({"nodes":[{"id":"t","name":"Trigger","kind":"trigger","config":{"trigger_kind":"schedule"}}],"edges":[]});
    assert!(
        proxy
            .call::<portability::TriggerDetails>(
                "DescribeTrigger",
                (portability::DescribeTrigger {
                    graph: missing_schedule
                },)
            )
            .await
            .is_err(),
        "missing schedule metadata must be a module error"
    );
    let malformed_schedule = json!({"nodes":[{"id":"t","name":"Trigger","kind":"trigger","config":{
        "trigger_kind":"schedule","schedule":{"kind":"every","every_ms":"soon"}
    }}],"edges":[]});
    assert!(
        proxy
            .call::<portability::TriggerDetails>(
                "DescribeTrigger",
                (portability::DescribeTrigger {
                    graph: malformed_schedule
                },)
            )
            .await
            .is_err(),
        "malformed schedule metadata must be a module error"
    );

    let graph = json!({"name":"event","nodes":[{"id":"t","name":"Trigger","kind":"trigger","config":{
        "trigger_kind":"app_event","toolkit":"GitHub","trigger_slug":"ISSUE_CREATED",
        "inputs":{"literal":"=value"}
    }}],"edges":[]});
    let matches: bool = proxy
        .call(
            "AppEventBinding",
            (portability::AppEventBinding {
                graph: graph.clone(),
                toolkit: "github".into(),
                trigger_slug: "issue_created".into(),
            },),
        )
        .await
        .unwrap();
    assert!(matches);
    let inputs: Value = proxy
        .call("PinnedInputs", (portability::PinnedInputs { graph },))
        .await
        .unwrap();
    assert_eq!(inputs, json!({"literal":"=value"}));
}
