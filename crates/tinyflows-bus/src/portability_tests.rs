use super::*;
use serde_json::json;

#[test]
fn portability_wire_requests_round_trip_without_credential_diagnostics() {
    let request = PrepareHttpRequest {
        request: json!({"headers":null}),
        credential: HttpCredentialTemplate::Bearer {
            secret: "secret".into(),
        },
    };
    let wire = serde_json::to_value(&request).unwrap();
    assert_eq!(wire["credential"]["scheme"], "bearer");
    assert!(serde_json::from_value::<PrepareHttpRequest>(wire).unwrap() == request);
    let binding = AppEventBinding {
        graph: json!({"nodes":[]}),
        toolkit: "github".into(),
        trigger_slug: "ISSUE_CREATED".into(),
    };
    assert_eq!(
        serde_json::from_value::<AppEventBinding>(serde_json::to_value(&binding).unwrap()).unwrap(),
        binding
    );
    let pinned = PinnedInputs {
        graph: json!({"nodes":[]}),
    };
    assert_eq!(
        serde_json::from_value::<PinnedInputs>(serde_json::to_value(&pinned).unwrap()).unwrap(),
        pinned
    );
}

#[test]
fn every_credential_template_has_a_stable_tagged_wire_shape() {
    let cases = [
        (
            HttpCredentialTemplate::Bearer {
                secret: "token".into(),
            },
            json!({"scheme":"bearer","secret":"token"}),
        ),
        (
            HttpCredentialTemplate::Basic {
                username: "alice".into(),
                secret: "secret".into(),
            },
            json!({"scheme":"basic","username":"alice","secret":"secret"}),
        ),
        (
            HttpCredentialTemplate::Header {
                header: "X-Key".into(),
                secret: "secret".into(),
            },
            json!({"scheme":"header","header":"X-Key","secret":"secret"}),
        ),
        (
            HttpCredentialTemplate::Resolved {
                header: "Authorization".into(),
                value: "Bearer token".into(),
            },
            json!({"scheme":"resolved","header":"Authorization","value":"Bearer token"}),
        ),
    ];
    for (template, expected) in cases {
        let wire = serde_json::to_value(&template).unwrap();
        assert_eq!(wire, expected);
        assert!(serde_json::from_value::<HttpCredentialTemplate>(wire).unwrap() == template);
    }
}

#[test]
fn trigger_details_wire_preserves_typed_schedule_variants_and_absence() {
    use crate::schedule::Schedule;

    let request = DescribeTrigger {
        graph: json!({"nodes":[]}),
    };
    assert_eq!(
        serde_json::to_value(&request).unwrap(),
        json!({"graph":{"nodes":[]}})
    );

    let schedules = [
        json!({"kind":"cron","expr":"0 9 * * 1","tz":"UTC"}),
        json!({"kind":"at","at":"2027-01-01T00:00:00Z"}),
        json!({"kind":"every","every_ms":60000}),
    ];
    for raw in schedules {
        let schedule: Schedule = serde_json::from_value(raw.clone()).unwrap();
        let expected_schedule = serde_json::to_value(&schedule).unwrap();
        let details = TriggerDetails {
            kind: Some("schedule".into()),
            schedule: Some(schedule),
        };
        let wire = serde_json::to_value(&details).unwrap();
        assert_eq!(wire["kind"], "schedule");
        assert_eq!(wire["schedule"], expected_schedule);
        assert_eq!(
            serde_json::from_value::<TriggerDetails>(wire).unwrap(),
            details
        );
    }

    let absent = TriggerDetails {
        kind: None,
        schedule: None,
    };
    let wire = serde_json::to_value(&absent).unwrap();
    assert_eq!(wire, json!({"kind":null,"schedule":null}));
    assert_eq!(
        serde_json::from_value::<TriggerDetails>(wire).unwrap(),
        absent
    );
}
