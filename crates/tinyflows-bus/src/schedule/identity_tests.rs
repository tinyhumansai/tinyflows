use super::*;
use serde_json::json;

fn channel_identity() -> ChannelSessionIdentity {
    ChannelSessionIdentity {
        tenant_key: "tenant-1".into(),
        agent_id: "agent-1".into(),
        session_key: "session-1".into(),
        provider: "telegram".into(),
        account_id: Some("account-1".into()),
        conversation: ChannelConversationIdentity {
            kind: "group".into(),
            id: "chat-1".into(),
            scope_id: Some("tenant-1".into()),
            parent_id: None,
            thread_id: Some("thread-1".into()),
            topic_id: Some("topic-1".into()),
        },
    }
}

#[test]
fn channel_session_identity_roundtrips_with_conversation_scope() {
    let identity = channel_identity();
    let value = serde_json::to_value(&identity).unwrap();

    assert_eq!(
        value,
        json!({
            "tenant_key": "tenant-1",
            "agent_id": "agent-1",
            "session_key": "session-1",
            "provider": "telegram",
            "account_id": "account-1",
            "conversation": {
                "kind": "group",
                "id": "chat-1",
                "scope_id": "tenant-1",
                "thread_id": "thread-1",
                "topic_id": "topic-1"
            }
        })
    );
    assert_eq!(
        serde_json::from_value::<ChannelSessionIdentity>(value).unwrap(),
        identity
    );
}

#[test]
fn channel_job_origin_carries_optional_session_identity_without_changing_legacy_wire() {
    let origin = JobOrigin::Channel {
        channel: "telegram".into(),
        reply_target: "chat-1".into(),
        history_key: "telegram:chat-1".into(),
        sender: None,
        thread_id: None,
        channel_session: Some(Box::new(channel_identity())),
    };
    let value = serde_json::to_value(&origin).unwrap();
    assert_eq!(value["kind"], "channel");
    assert_eq!(value["channel_session"]["session_key"], "session-1");
    assert_eq!(serde_json::from_value::<JobOrigin>(value).unwrap(), origin);

    let legacy = json!({
        "kind": "channel",
        "channel": "discord",
        "reply_target": "room-1",
        "history_key": "discord:room-1"
    });
    let parsed: JobOrigin = serde_json::from_value(legacy.clone()).unwrap();
    assert_eq!(
        parsed,
        JobOrigin::Channel {
            channel: "discord".into(),
            reply_target: "room-1".into(),
            history_key: "discord:room-1".into(),
            sender: None,
            thread_id: None,
            channel_session: None,
        }
    );
    assert_eq!(serde_json::to_value(parsed).unwrap(), legacy);
}

#[test]
fn channel_origin_accepts_session_identity_wire_alias() {
    let origin: JobOrigin = serde_json::from_value(json!({
        "kind": "channel",
        "channel": "telegram",
        "reply_target": "chat-1",
        "history_key": "telegram:chat-1",
        "session_identity": channel_identity(),
    }))
    .unwrap();

    assert!(matches!(
        origin,
        JobOrigin::Channel {
            channel_session: Some(_),
            ..
        }
    ));
}
