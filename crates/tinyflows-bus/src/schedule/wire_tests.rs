//! Literal-JSON fixtures pinning the persisted / RPC wire shape of the
//! schedule types. These strings are what a host's job store and RPC clients
//! already hold; if one stops parsing or re-serialising byte-for-byte, a
//! stored job has been orphaned.

use super::{
    ActiveHours, ChannelConversationIdentity, ChannelSessionIdentity, CronJob, CronJobPatch,
    CronRun, DeliveryConfig, DeliveryStatus, JobOrigin, JobType, Schedule, SessionTarget,
};

fn assert_bytes<T>(json: &str, expected: &T)
where
    T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let parsed: T = serde_json::from_str(json).expect("fixture parses");
    assert_eq!(&parsed, expected);
    assert_eq!(serde_json::to_string(&parsed).unwrap(), json);
}

#[test]
fn cron_schedule_wire_bytes() {
    assert_bytes(
        r#"{"kind":"cron","expr":"0 9 * * 1","tz":"Europe/London","active_hours":{"start":"09:00","end":"17:30"}}"#,
        &Schedule::Cron {
            expr: "0 9 * * 1".into(),
            tz: Some("Europe/London".into()),
            active_hours: Some(ActiveHours {
                start: "09:00".into(),
                end: "17:30".into(),
            }),
        },
    );
    assert_bytes(
        r#"{"kind":"cron","expr":"*/5 * * * *","tz":null,"active_hours":null}"#,
        &Schedule::Cron {
            expr: "*/5 * * * *".into(),
            tz: None,
            active_hours: None,
        },
    );
}

#[test]
fn at_and_every_schedule_wire_bytes() {
    let at: Schedule =
        serde_json::from_str(r#"{"kind":"at","at":"2026-02-16T17:00:00Z"}"#).unwrap();
    assert!(matches!(at, Schedule::At { .. }));
    assert_eq!(
        serde_json::to_string(&at).unwrap(),
        r#"{"kind":"at","at":"2026-02-16T17:00:00Z"}"#
    );
    assert_bytes(
        r#"{"kind":"every","every_ms":60000}"#,
        &Schedule::Every { every_ms: 60_000 },
    );
}

#[test]
fn legacy_shapes_still_deserialize() {
    // Missing optional keys and the bare-string shorthand both read as Cron.
    let no_optionals: Schedule =
        serde_json::from_str(r#"{"kind":"cron","expr":"0 9 * * *"}"#).unwrap();
    let bare: Schedule = serde_json::from_str(r#""0 9 * * *""#).unwrap();
    let expected = Schedule::Cron {
        expr: "0 9 * * *".into(),
        tz: None,
        active_hours: None,
    };
    assert_eq!(no_optionals, expected);
    assert_eq!(bare, expected);
}

#[test]
fn enum_and_delivery_wire_bytes() {
    assert_bytes(r#""shell""#, &JobType::Shell);
    assert_bytes(r#""agent""#, &JobType::Agent);
    assert_bytes(r#""flow""#, &JobType::Flow);
    assert_bytes(r#""isolated""#, &SessionTarget::Isolated);
    assert_bytes(r#""main""#, &SessionTarget::Main);
    assert_bytes(r#""current""#, &SessionTarget::Current);
    assert_bytes(r#""not_requested""#, &DeliveryStatus::NotRequested);
    assert_bytes(
        r#"{"mode":"none","channel":null,"to":null,"best_effort":true}"#,
        &DeliveryConfig::default(),
    );
}

#[test]
fn job_and_run_wire_bytes() {
    let job_json = r#"{"id":"j1","expression":"0 9 * * *","schedule":{"kind":"cron","expr":"0 9 * * *","tz":null,"active_hours":null},"command":"echo hi","prompt":null,"name":"morning","job_type":"shell","session_target":"isolated","model":null,"agent_id":null,"enabled":true,"delivery":{"mode":"none","channel":null,"to":null,"best_effort":true},"delete_after_run":false,"created_at":"2026-02-16T00:00:00Z","next_run":"2026-02-16T09:00:00Z","last_run":null,"last_status":null,"last_output":null}"#;
    let job: CronJob = serde_json::from_str(job_json).unwrap();
    assert_eq!(serde_json::to_string(&job).unwrap(), job_json);

    let run_json = r#"{"id":7,"job_id":"j1","started_at":"2026-02-16T09:00:00Z","finished_at":"2026-02-16T09:00:02Z","status":"ok","output":"hi","duration_ms":2000}"#;
    let run: CronRun = serde_json::from_str(run_json).unwrap();
    assert_eq!(serde_json::to_string(&run).unwrap(), run_json);
}

#[test]
fn patch_double_option_wire_semantics() {
    let absent: CronJobPatch = serde_json::from_str("{}").unwrap();
    assert_eq!(absent.agent_id, None);
    let cleared: CronJobPatch = serde_json::from_str(r#"{"agent_id":null}"#).unwrap();
    assert_eq!(cleared.agent_id, Some(None));
    let set: CronJobPatch = serde_json::from_str(r#"{"agent_id":"welcome"}"#).unwrap();
    assert_eq!(set.agent_id, Some(Some("welcome".into())));
}

#[test]
fn origin_wire_bytes() {
    assert_bytes(
        r#"{"kind":"web","thread_id":"t1","agent_id":"orchestrator"}"#,
        &JobOrigin::Web {
            thread_id: "t1".into(),
            agent_id: Some("orchestrator".into()),
        },
    );
    assert_bytes(
        r#"{"kind":"channel","channel":"telegram","reply_target":"42","history_key":"telegram:42","sender":"alice","thread_id":"7"}"#,
        &JobOrigin::Channel {
            channel: "telegram".into(),
            reply_target: "42".into(),
            history_key: "telegram:42".into(),
            sender: Some("alice".into()),
            thread_id: Some("7".into()),
            channel_session: None,
        },
    );
}

#[test]
fn channel_origin_with_channel_session_wire_bytes() {
    assert_bytes(
        r#"{"kind":"channel","channel":"telegram","reply_target":"42","history_key":"telegram:42","channel_session":{"tenant_key":"tenant-1","agent_id":"agent-1","session_key":"session-1","provider":"telegram","conversation":{"kind":"group","id":"chat-1","thread_id":"thread-1"}}}"#,
        &JobOrigin::Channel {
            channel: "telegram".into(),
            reply_target: "42".into(),
            history_key: "telegram:42".into(),
            sender: None,
            thread_id: None,
            channel_session: Some(Box::new(ChannelSessionIdentity {
                tenant_key: "tenant-1".into(),
                agent_id: "agent-1".into(),
                session_key: "session-1".into(),
                provider: "telegram".into(),
                account_id: None,
                conversation: ChannelConversationIdentity {
                    kind: "group".into(),
                    id: "chat-1".into(),
                    scope_id: None,
                    parent_id: None,
                    thread_id: Some("thread-1".into()),
                    topic_id: None,
                },
            })),
        },
    );
}

#[test]
fn job_with_origin_and_run_with_delivery_status_wire_bytes() {
    let job_json = r#"{"id":"j1","expression":"0 9 * * *","schedule":{"kind":"cron","expr":"0 9 * * *","tz":null,"active_hours":null},"command":"","prompt":"brief","name":null,"job_type":"agent","session_target":"current","model":null,"agent_id":null,"enabled":true,"delivery":{"mode":"origin","channel":null,"to":null,"best_effort":true},"delete_after_run":false,"created_at":"2026-02-16T00:00:00Z","next_run":"2026-02-16T09:00:00Z","last_run":null,"last_status":null,"last_output":null,"origin":{"kind":"web","thread_id":"t1"}}"#;
    let job: CronJob = serde_json::from_str(job_json).unwrap();
    assert_eq!(serde_json::to_string(&job).unwrap(), job_json);

    let run_json = r#"{"id":7,"job_id":"j1","started_at":"2026-02-16T09:00:00Z","finished_at":"2026-02-16T09:00:02Z","status":"ok","output":"hi","duration_ms":2000,"delivery_status":"delivered"}"#;
    let run: CronRun = serde_json::from_str(run_json).unwrap();
    assert_eq!(serde_json::to_string(&run).unwrap(), run_json);
}

/// A patch that does not mention the agent definition must still not mention it
/// after a trip over the wire; only an explicit clear writes `null`.
#[test]
fn an_unrelated_patch_does_not_clear_the_agent_definition_on_a_round_trip() {
    let untouched = CronJobPatch {
        enabled: Some(false),
        ..CronJobPatch::default()
    };
    let wire = serde_json::to_value(&untouched).unwrap();
    assert!(wire.get("agent_id").is_none(), "{wire}");
    let back: CronJobPatch = serde_json::from_value(wire).unwrap();
    assert_eq!(back.agent_id, None);

    let cleared = CronJobPatch {
        agent_id: Some(None),
        ..CronJobPatch::default()
    };
    let wire = serde_json::to_value(&cleared).unwrap();
    assert_eq!(wire["agent_id"], serde_json::Value::Null);
    let back: CronJobPatch = serde_json::from_value(wire).unwrap();
    assert_eq!(back.agent_id, Some(None));
}
