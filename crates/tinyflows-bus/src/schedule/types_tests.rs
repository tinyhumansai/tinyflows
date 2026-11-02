use super::*;
use crate::schedule::ChannelConversationIdentity;
use chrono::TimeZone;
use serde_json::json;

// ── JobType ────────────────────────────────────────────────────

#[test]
fn job_type_parse_and_as_str_roundtrip() {
    assert_eq!(JobType::parse("shell").as_str(), "shell");
    assert_eq!(JobType::parse("agent").as_str(), "agent");
    assert_eq!(JobType::parse("flow").as_str(), "flow");
    // Case-insensitive
    assert_eq!(JobType::parse("AGENT"), JobType::Agent);
    assert_eq!(JobType::parse("Agent"), JobType::Agent);
    assert_eq!(JobType::parse("FLOW"), JobType::Flow);
    // Anything unknown falls back to Shell (the default) — guards
    // against unexpected legacy DB rows silently turning into Agent.
    assert_eq!(JobType::parse(""), JobType::Shell);
    assert_eq!(JobType::parse("garbage"), JobType::Shell);
}

#[test]
fn job_type_default_is_shell() {
    assert_eq!(JobType::default(), JobType::Shell);
}

#[test]
fn job_type_serializes_lowercase() {
    assert_eq!(serde_json::to_string(&JobType::Shell).unwrap(), "\"shell\"");
    assert_eq!(serde_json::to_string(&JobType::Agent).unwrap(), "\"agent\"");
}

// ── SessionTarget ──────────────────────────────────────────────

#[test]
fn session_target_parse_and_as_str_roundtrip() {
    assert_eq!(SessionTarget::parse("isolated").as_str(), "isolated");
    assert_eq!(SessionTarget::parse("main").as_str(), "main");
    // Case-insensitive + unknown falls back to Isolated (the default).
    assert_eq!(SessionTarget::parse("MAIN"), SessionTarget::Main);
    assert_eq!(SessionTarget::parse(""), SessionTarget::Isolated);
    assert_eq!(SessionTarget::parse("unknown"), SessionTarget::Isolated);
}

#[test]
fn session_target_default_is_isolated() {
    assert_eq!(SessionTarget::default(), SessionTarget::Isolated);
}

#[test]
fn session_target_serializes_lowercase() {
    assert_eq!(
        serde_json::to_string(&SessionTarget::Isolated).unwrap(),
        "\"isolated\""
    );
    assert_eq!(
        serde_json::to_string(&SessionTarget::Main).unwrap(),
        "\"main\""
    );
}

// ── Schedule ───────────────────────────────────────────────────

#[test]
fn schedule_cron_variant_roundtrips_with_optional_tz() {
    let s = Schedule::Cron {
        expr: "0 9 * * *".into(),
        tz: Some("America/Los_Angeles".into()),
        active_hours: None,
    };
    let v = serde_json::to_value(&s).unwrap();
    assert_eq!(v["kind"], "cron");
    assert_eq!(v["expr"], "0 9 * * *");
    assert_eq!(v["tz"], "America/Los_Angeles");
    let back: Schedule = serde_json::from_value(v).unwrap();
    assert_eq!(back, s);
}

#[test]
fn schedule_cron_variant_accepts_missing_tz() {
    let raw = json!({ "kind": "cron", "expr": "*/5 * * * *" });
    let s: Schedule = serde_json::from_value(raw).unwrap();
    assert_eq!(
        s,
        Schedule::Cron {
            expr: "*/5 * * * *".into(),
            tz: None,
            active_hours: None,
        }
    );
}

#[test]
fn schedule_cron_variant_roundtrips_with_active_hours() {
    let s = Schedule::Cron {
        expr: "*/15 * * * *".into(),
        tz: Some("UTC".into()),
        active_hours: Some(ActiveHours {
            start: "09:00".into(),
            end: "17:30".into(),
        }),
    };
    let v = serde_json::to_value(&s).unwrap();
    assert_eq!(v["active_hours"]["start"], "09:00");
    assert_eq!(v["active_hours"]["end"], "17:30");
    let back: Schedule = serde_json::from_value(v).unwrap();
    assert_eq!(back, s);
}

#[test]
fn schedule_at_variant_roundtrips_with_utc_timestamp() {
    let at = Utc.with_ymd_and_hms(2027, 1, 15, 12, 0, 0).unwrap();
    let s = Schedule::At { at };
    let v = serde_json::to_value(&s).unwrap();
    assert_eq!(v["kind"], "at");
    let back: Schedule = serde_json::from_value(v).unwrap();
    assert_eq!(back, s);
}

#[test]
fn schedule_every_variant_roundtrips() {
    let s = Schedule::Every { every_ms: 60_000 };
    let v = serde_json::to_value(&s).unwrap();
    assert_eq!(v["kind"], "every");
    assert_eq!(v["every_ms"], 60_000);
    let back: Schedule = serde_json::from_value(v).unwrap();
    assert_eq!(back, s);
}

// ── Schedule bare-string deserialization (CORE-RUST-FY fix) ──────
// Callers (agents, older frontend) sometimes pass a bare cron
// expression string like `"0 9 * * 1"` instead of the structured
// `{"kind":"cron","expr":"0 9 * * 1"}` form.  Both must parse.

#[test]
fn schedule_deserializes_bare_cron_string() {
    let s: Schedule = serde_json::from_value(json!("0 9 * * 1")).unwrap();
    assert_eq!(
        s,
        Schedule::Cron {
            expr: "0 9 * * 1".into(),
            tz: None,
            active_hours: None,
        }
    );
}

#[test]
fn schedule_deserializes_bare_5_field_cron_string() {
    let s: Schedule = serde_json::from_str("\"*/5 * * * *\"").unwrap();
    assert_eq!(
        s,
        Schedule::Cron {
            expr: "*/5 * * * *".into(),
            tz: None,
            active_hours: None,
        }
    );
}

#[test]
fn cron_job_patch_accepts_bare_schedule_string() {
    // This is the exact payload shape that triggered CORE-RUST-FY:
    // {"schedule": "0 9 * * 1"}
    let raw = json!({ "schedule": "0 9 * * 1" });
    let patch: CronJobPatch = serde_json::from_value(raw).unwrap();
    assert_eq!(
        patch.schedule,
        Some(Schedule::Cron {
            expr: "0 9 * * 1".into(),
            tz: None,
            active_hours: None,
        })
    );
}

#[test]
fn cron_job_patch_still_accepts_structured_schedule_object() {
    let raw = json!({ "schedule": { "kind": "cron", "expr": "0 9 * * 1" } });
    let patch: CronJobPatch = serde_json::from_value(raw).unwrap();
    assert_eq!(
        patch.schedule,
        Some(Schedule::Cron {
            expr: "0 9 * * 1".into(),
            tz: None,
            active_hours: None,
        })
    );
}

// ── DeliveryConfig ─────────────────────────────────────────────

#[test]
fn delivery_config_default_is_none_mode_best_effort() {
    let d = DeliveryConfig::default();
    assert_eq!(d.mode, "none");
    assert!(d.channel.is_none());
    assert!(d.to.is_none());
    assert!(d.best_effort, "default best_effort must be true");
}

#[test]
fn delivery_config_parses_empty_object_with_defaults() {
    // A bare `{}` must deserialize with the `#[serde(default)]` / default
    // fn fallbacks — otherwise legacy rows without delivery fields would
    // fail to load.
    let d: DeliveryConfig = serde_json::from_str("{}").unwrap();
    assert_eq!(d.mode, "");
    assert!(d.channel.is_none());
    assert!(d.to.is_none());
    assert!(d.best_effort, "best_effort must default to true");
}

#[test]
fn delivery_config_preserves_best_effort_false_override() {
    let raw = json!({ "mode": "channel", "best_effort": false });
    let d: DeliveryConfig = serde_json::from_value(raw).unwrap();
    assert_eq!(d.mode, "channel");
    assert!(!d.best_effort);
}

// ── CronJobPatch ───────────────────────────────────────────────

#[test]
fn cron_job_patch_default_is_all_none() {
    let p = CronJobPatch::default();
    assert!(p.schedule.is_none());
    assert!(p.command.is_none());
    assert!(p.prompt.is_none());
    assert!(p.name.is_none());
    assert!(p.enabled.is_none());
    assert!(p.delivery.is_none());
    assert!(p.model.is_none());
    assert!(p.session_target.is_none());
    assert!(p.delete_after_run.is_none());
    assert!(p.agent_id.is_none());
}

#[test]
fn patch_agent_id_wire_double_option_semantics() {
    // Same fix applied consistently to `agent_id` (its doc + the struct-level
    // clearing test already document the Some(None)=clear intent).
    let absent: CronJobPatch = serde_json::from_value(json!({})).unwrap();
    assert_eq!(absent.agent_id, None, "absent key means no change");
    let cleared: CronJobPatch = serde_json::from_value(json!({ "agent_id": null })).unwrap();
    assert_eq!(
        cleared.agent_id,
        Some(None),
        "wire null must clear the agent definition"
    );
    let set: CronJobPatch = serde_json::from_value(json!({ "agent_id": "welcome" })).unwrap();
    assert_eq!(set.agent_id, Some(Some("welcome".to_string())));
}

#[test]
fn cron_job_patch_agent_id_supports_explicit_none_clearing() {
    // Option<Option<String>> lets callers distinguish "no change"
    // (None) from "clear the agent_id" (Some(None)).
    let p = CronJobPatch {
        agent_id: Some(None),
        ..Default::default()
    };
    assert!(p.agent_id.is_some());
    assert!(p.agent_id.as_ref().unwrap().is_none());
}

// ── SessionTarget::Current ─────────────────────────────────────

#[test]
fn session_target_parses_current_main_isolated_and_unknown() {
    assert_eq!(SessionTarget::parse("current"), SessionTarget::Current);
    assert_eq!(SessionTarget::parse("CURRENT"), SessionTarget::Current);
    assert_eq!(SessionTarget::parse("main"), SessionTarget::Main);
    assert_eq!(SessionTarget::parse("isolated"), SessionTarget::Isolated);
    assert_eq!(SessionTarget::parse("bogus"), SessionTarget::Isolated);
    assert_eq!(SessionTarget::Current.as_str(), "current");
}

#[test]
fn session_target_current_serializes_as_current() {
    assert_eq!(
        serde_json::to_string(&SessionTarget::Current).unwrap(),
        "\"current\""
    );
    let back: SessionTarget = serde_json::from_str("\"current\"").unwrap();
    assert_eq!(back, SessionTarget::Current);
}

// ── JobOrigin ──────────────────────────────────────────────────

#[test]
fn job_origin_web_roundtrips_and_omits_absent_agent_id() {
    let origin = JobOrigin::Web {
        thread_id: "t-1".into(),
        agent_id: None,
    };
    let v = serde_json::to_value(&origin).unwrap();
    assert_eq!(v, json!({ "kind": "web", "thread_id": "t-1" }));
    let back: JobOrigin = serde_json::from_value(v).unwrap();
    assert_eq!(back, origin);
    assert_eq!(origin.kind_str(), "web");

    let with_agent = JobOrigin::Web {
        thread_id: "t-2".into(),
        agent_id: Some("orchestrator".into()),
    };
    let v = serde_json::to_value(&with_agent).unwrap();
    assert_eq!(v["agent_id"], "orchestrator");
    assert_eq!(serde_json::from_value::<JobOrigin>(v).unwrap(), with_agent);
}

#[test]
fn job_origin_channel_roundtrips_with_and_without_optionals() {
    let full = JobOrigin::Channel {
        channel: "telegram".into(),
        reply_target: "chat-42".into(),
        history_key: "telegram:chat-42".into(),
        sender: Some("alice".into()),
        thread_id: Some("topic-7".into()),
        channel_session: Some(Box::new(ChannelSessionIdentity {
            tenant_key: "profile:v1:local".into(),
            agent_id: "telegram-agent".into(),
            session_key: "main:telegram:account-a:group:room-42:topic:topic-7".into(),
            provider: "telegram".into(),
            account_id: Some("account-a".into()),
            conversation: ChannelConversationIdentity {
                kind: "group".into(),
                id: "room-42".into(),
                scope_id: Some("workspace-3".into()),
                parent_id: Some("parent-9".into()),
                thread_id: Some("topic-7".into()),
                topic_id: Some("topic-7".into()),
            },
        })),
    };
    let v = serde_json::to_value(&full).unwrap();
    assert_eq!(v["kind"], "channel");
    assert_eq!(v["sender"], "alice");
    assert_eq!(v["thread_id"], "topic-7");
    assert_eq!(serde_json::from_value::<JobOrigin>(v).unwrap(), full);
    assert_eq!(full.kind_str(), "channel");

    let minimal = json!({
        "kind": "channel",
        "channel": "discord",
        "reply_target": "c1",
        "history_key": "discord:c1"
    });
    let parsed: JobOrigin = serde_json::from_value(minimal.clone()).unwrap();
    assert_eq!(
        parsed,
        JobOrigin::Channel {
            channel: "discord".into(),
            reply_target: "c1".into(),
            history_key: "discord:c1".into(),
            sender: None,
            thread_id: None,
            channel_session: None,
        }
    );
    assert_eq!(serde_json::to_value(&parsed).unwrap(), minimal);
}

// ── CronJob.origin ─────────────────────────────────────────────

fn legacy_job_json() -> serde_json::Value {
    json!({
        "id": "j1",
        "expression": "0 9 * * *",
        "schedule": { "kind": "cron", "expr": "0 9 * * *" },
        "command": "",
        "prompt": "brief me",
        "name": null,
        "job_type": "agent",
        "session_target": "isolated",
        "model": null,
        "agent_id": null,
        "enabled": true,
        "delivery": { "mode": "none" },
        "delete_after_run": false,
        "created_at": "2026-02-16T00:00:00Z",
        "next_run": "2026-02-16T09:00:00Z",
        "last_run": null,
        "last_status": null,
        "last_output": null
    })
}

#[test]
fn legacy_cron_job_without_origin_deserializes_to_none() {
    let job: CronJob = serde_json::from_value(legacy_job_json()).unwrap();
    assert_eq!(job.origin, None);
    let v = serde_json::to_value(&job).unwrap();
    assert!(v.get("origin").is_none(), "absent origin is not serialized");
}

#[test]
fn cron_job_with_origin_roundtrips() {
    let mut raw = legacy_job_json();
    raw["session_target"] = json!("current");
    raw["origin"] = json!({ "kind": "web", "thread_id": "t-9" });
    let job: CronJob = serde_json::from_value(raw).unwrap();
    assert_eq!(job.session_target, SessionTarget::Current);
    assert_eq!(
        job.origin,
        Some(JobOrigin::Web {
            thread_id: "t-9".into(),
            agent_id: None,
        })
    );
    let v = serde_json::to_value(&job).unwrap();
    assert_eq!(v["origin"], json!({ "kind": "web", "thread_id": "t-9" }));
}

// ── CronJobPatch.origin ────────────────────────────────────────

#[test]
fn patch_origin_wire_double_option_semantics() {
    let absent: CronJobPatch = serde_json::from_value(json!({})).unwrap();
    assert_eq!(absent.origin, None, "absent key means no change");
    let cleared: CronJobPatch = serde_json::from_value(json!({ "origin": null })).unwrap();
    assert_eq!(cleared.origin, Some(None), "wire null clears the origin");
    let set: CronJobPatch =
        serde_json::from_value(json!({ "origin": { "kind": "web", "thread_id": "t" } })).unwrap();
    assert_eq!(
        set.origin,
        Some(Some(JobOrigin::Web {
            thread_id: "t".into(),
            agent_id: None,
        }))
    );
}

// ── Delivery mode + status ─────────────────────────────────────

#[test]
fn delivery_mode_constants_are_the_wire_strings() {
    assert_eq!(delivery_mode::NONE, "none");
    assert_eq!(delivery_mode::ANNOUNCE, "announce");
    assert_eq!(delivery_mode::PROACTIVE, "proactive");
    assert_eq!(delivery_mode::ORIGIN, "origin");
    assert_eq!(DeliveryConfig::default().mode, delivery_mode::NONE);
}

#[test]
fn delivery_status_parse_as_str_and_serde_agree() {
    for (status, wire) in [
        (DeliveryStatus::Delivered, "delivered"),
        (DeliveryStatus::Suppressed, "suppressed"),
        (DeliveryStatus::Failed, "failed"),
        (DeliveryStatus::NotRequested, "not_requested"),
    ] {
        assert_eq!(status.as_str(), wire);
        assert_eq!(DeliveryStatus::parse(wire), Some(status.clone()));
        assert_eq!(
            serde_json::to_string(&status).unwrap(),
            format!("\"{wire}\"")
        );
    }
    assert_eq!(
        DeliveryStatus::parse("DELIVERED"),
        Some(DeliveryStatus::Delivered)
    );
    assert_eq!(DeliveryStatus::parse("bogus"), None);
}

// ── CronRun.delivery_status ────────────────────────────────────

#[test]
fn cron_run_delivery_status_is_optional_on_the_wire() {
    let legacy = json!({
        "id": 1, "job_id": "j1",
        "started_at": "2026-02-16T09:00:00Z", "finished_at": "2026-02-16T09:00:01Z",
        "status": "ok", "output": null, "duration_ms": 1000
    });
    let run: CronRun = serde_json::from_value(legacy).unwrap();
    assert_eq!(run.delivery_status, None);
    assert!(
        serde_json::to_value(&run)
            .unwrap()
            .get("delivery_status")
            .is_none()
    );

    let mut with = serde_json::to_value(&run).unwrap();
    with["delivery_status"] = json!("delivered");
    let run: CronRun = serde_json::from_value(with).unwrap();
    assert_eq!(run.delivery_status, Some(DeliveryStatus::Delivered));
}

#[test]
fn patch_without_origin_roundtrips_as_no_change() {
    let patch = CronJobPatch {
        enabled: Some(false),
        ..Default::default()
    };
    let v = serde_json::to_value(&patch).unwrap();
    assert!(v.get("origin").is_none(), "absent origin must be omitted");
    let back: CronJobPatch = serde_json::from_value(v).unwrap();
    assert_eq!(back.origin, None);
}

#[test]
fn patch_clearing_origin_serializes_explicit_null() {
    let patch = CronJobPatch {
        origin: Some(None),
        ..Default::default()
    };
    let v = serde_json::to_value(&patch).unwrap();
    assert!(v.get("origin").is_some_and(|o| o.is_null()));
    let back: CronJobPatch = serde_json::from_value(v).unwrap();
    assert_eq!(back.origin, Some(None));
}
