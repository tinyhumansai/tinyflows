use super::*;

#[test]
fn public_facade_reexports_channel_identity_types() {
    let _: Option<crate::ChannelSessionIdentity> = None;
    let _: Option<crate::ChannelConversationIdentity> = None;
}

#[test]
fn delay_units_and_default_minutes_match_existing_jobs() {
    for (text, secs) in [
        ("5s", 5),
        (" 7m ", 420),
        ("2h", 7200),
        ("3d", 259200),
        ("15", 900),
    ] {
        assert_eq!(parse_human_delay(text).unwrap().num_seconds(), secs);
    }
    for text in ["", " ", "abc", "5x", "5wk", "9223372036854775807d"] {
        assert!(parse_human_delay(text).is_err(), "{text}");
    }
}

#[test]
fn announce_requires_target_and_enforces_channel_recipients() {
    let mut delivery = DeliveryConfig {
        mode: "announce".into(),
        ..Default::default()
    };
    assert!(validate_delivery(&delivery, None, Some(&[])).is_err());
    delivery.channel = Some("telegram".into());
    delivery.to = Some("alice".into());
    assert!(validate_delivery(&delivery, None, None).is_err());
    assert!(validate_delivery(&delivery, None, Some(&["bob".into()])).is_err());
    assert!(validate_delivery(&delivery, None, Some(&["alice".into()])).is_ok());
    assert!(validate_delivery(&delivery, None, Some(&[])).is_ok());
}

#[test]
fn origin_requires_conversation_and_announce_can_reply_to_its_asker() {
    let delivery = DeliveryConfig {
        mode: "origin".into(),
        ..Default::default()
    };
    assert!(validate_delivery(&delivery, None, None).is_err());
    assert!(
        check_origin_requirements(&SessionTarget::Current, &DeliveryConfig::default(), None)
            .is_err()
    );
    let origin = JobOrigin::Channel {
        channel: "Telegram".into(),
        reply_target: "alice".into(),
        history_key: "conversation".into(),
        sender: None,
        thread_id: None,
        channel_session: None,
    };
    assert!(validate_delivery(&delivery, Some(&origin), None).is_ok());
    let announce = DeliveryConfig {
        mode: "announce".into(),
        channel: Some("telegram".into()),
        to: Some("alice".into()),
        best_effort: true,
    };
    assert!(validate_delivery(&announce, Some(&origin), Some(&["bob".into()])).is_ok());
}

#[test]
fn default_delivery_targets_origin_only_when_one_exists() {
    let origin = JobOrigin::Web {
        thread_id: "t".into(),
        agent_id: None,
    };
    let with = default_delivery(Some(&origin));
    assert_eq!(with.mode, delivery_mode::ORIGIN);
    assert!(with.best_effort && with.channel.is_none() && with.to.is_none());
    assert_eq!(default_delivery(None).mode, delivery_mode::PROACTIVE);
}
