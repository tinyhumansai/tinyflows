//! Opaque channel-session identity captured by a host from validated inbound
//! channel metadata. TinyFlows stores and forwards it with job origins; the
//! host remains responsible for validating it before use.
use serde::{Deserialize, Serialize};

/// Stable identity of the channel session that created a scheduled job.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChannelSessionIdentity {
    /// Tenant or profile namespace selected by the host.
    pub tenant_key: String,
    /// Agent that owns the conversation.
    pub agent_id: String,
    /// Durable session key used to resume the transcript.
    pub session_key: String,
    /// Provider identifier, such as `telegram` or `slack`.
    pub provider: String,
    /// Account identity when the provider has multiple configured accounts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    /// Provider-scoped conversation and thread identity.
    pub conversation: ChannelConversationIdentity,
}

/// Provider-scoped identity of a channel conversation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChannelConversationIdentity {
    /// Conversation kind, such as `direct`, `group`, or `channel`.
    pub kind: String,
    /// Provider-defined conversation identifier.
    pub id: String,
    /// Optional tenant/workspace scope for providers whose ids are local.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_id: Option<String>,
    /// Parent conversation identifier, when the provider has nested spaces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// Thread identifier, when the conversation is threaded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    /// Topic identifier for providers that model topics separately.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic_id: Option<String>,
}
