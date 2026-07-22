//! Native companion state used by the TinyFlows Chrome extension relay.
//!
//! This module deliberately separates security and lifecycle policy from the
//! HTTP/WebSocket adapter. A server adapter must bind [`RelayPolicy::bind_addr`],
//! authenticate every upgrade with [`Authenticator`], and drive [`RelayState`]
//! for the lifetime of the authenticated socket.

mod auth;
mod control;
mod relay;
mod tabs;

pub use auth::{
    AuthError, AuthenticatedSession, Authenticator, PairingSecret, SecretStore, WebSocketHandshake,
};
pub use control::{CompanionControlRequest, CompanionControlResponse, RunEvent, WorkflowSummary};
pub use relay::{DisconnectOutcome, PendingAction, RelayError, RelayPolicy, RelayState, SessionId};
pub use tabs::{RunBinding, RunId, SharedTab, TabId, TabRegistry, TabRegistryError};
