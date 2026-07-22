//! Fail-closed relay connection, heartbeat, and action-correlation state.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use crate::browser::{
    BROWSER_PROTOCOL_VERSION, BrowserError, BrowserErrorCode, BrowserEvent, BrowserRequest,
    BrowserResponse,
};

use super::{RunId, TabId, TabRegistry, TabRegistryError};

/// Identifier for one authenticated extension WebSocket session.
pub type SessionId = String;

/// Security and deadline policy enforced by a relay adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayPolicy {
    /// Listener address. Construction guarantees this address is loopback-only.
    pub bind_addr: SocketAddr,
    /// Largest action timeout accepted from a workflow request.
    pub maximum_action_timeout: Duration,
    /// Maximum time without an authenticated heartbeat before fail-closed disconnect.
    pub heartbeat_timeout: Duration,
}

impl RelayPolicy {
    /// Creates policy for a loopback-only listener.
    pub fn loopback(port: u16) -> Self {
        Self {
            bind_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port),
            maximum_action_timeout: Duration::from_secs(60),
            heartbeat_timeout: Duration::from_secs(30),
        }
    }

    /// Validates policy supplied by an embedding host.
    pub fn validate(&self) -> Result<(), RelayError> {
        if !self.bind_addr.ip().is_loopback() {
            return Err(RelayError::NonLoopbackBind);
        }
        if self.maximum_action_timeout.is_zero() || self.heartbeat_timeout.is_zero() {
            return Err(RelayError::InvalidTimeout);
        }
        Ok(())
    }
}

/// One browser command awaiting a terminal extension response.
#[derive(Debug, Clone)]
pub struct PendingAction {
    /// Correlation id copied from the browser request.
    pub request_id: String,
    /// Run that owns the action.
    pub run_id: RunId,
    /// Explicit target tab.
    pub tab_id: TabId,
    /// Authenticated extension session that received the action.
    pub session_id: SessionId,
    /// Absolute timeout deadline.
    pub deadline: Instant,
}

/// Terminal outcomes produced when an authenticated relay disconnects.
#[derive(Debug, Clone, PartialEq)]
pub struct DisconnectOutcome {
    /// Browser lifecycle event broadcast to native run observers.
    pub event: BrowserEvent,
    /// Correlated failures for every action interrupted by the disconnect.
    pub responses: Vec<BrowserResponse>,
}

/// Relay state-machine failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayError {
    /// A listener policy attempted to expose the relay beyond loopback.
    NonLoopbackBind,
    /// A configured or requested timeout was zero or exceeded policy.
    InvalidTimeout,
    /// No authenticated extension session is connected.
    RelayDisconnected,
    /// A request used an unsupported protocol version.
    ProtocolMismatch,
    /// A request id is already pending.
    DuplicateRequestId,
    /// A response did not correlate to a pending request.
    UnknownRequestId,
    /// A response came from a session other than the one receiving the request.
    SessionMismatch,
    /// Explicit shared-tab policy rejected the request.
    Tab(TabRegistryError),
}

impl RelayError {
    /// Stable wire error code for the failure.
    pub fn browser_code(&self) -> BrowserErrorCode {
        match self {
            Self::RelayDisconnected => BrowserErrorCode::RelayDisconnected,
            Self::ProtocolMismatch => BrowserErrorCode::ProtocolMismatch,
            Self::InvalidTimeout => BrowserErrorCode::ActionTimeout,
            Self::Tab(TabRegistryError::TabNotShared | TabRegistryError::RunTabMismatch) => {
                BrowserErrorCode::TabNotShared
            }
            Self::Tab(TabRegistryError::TabRevoked) => BrowserErrorCode::TabRevoked,
            Self::Tab(TabRegistryError::UnsupportedPage) => BrowserErrorCode::UnsupportedPage,
            Self::DuplicateRequestId
            | Self::UnknownRequestId
            | Self::SessionMismatch
            | Self::NonLoopbackBind
            | Self::Tab(TabRegistryError::RunAlreadyBound) => BrowserErrorCode::InvalidRequest,
        }
    }
}

impl std::fmt::Display for RelayError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tab(error) => write!(formatter, "tab authorization failed: {error}"),
            other => formatter.write_str(match other {
                Self::NonLoopbackBind => "relay listener must bind to a loopback address",
                Self::InvalidTimeout => "relay timeout is outside the allowed bounds",
                Self::RelayDisconnected => "authenticated extension relay is disconnected",
                Self::ProtocolMismatch => "browser protocol version is unsupported",
                Self::DuplicateRequestId => "browser request id is already pending",
                Self::UnknownRequestId => "browser response has an unknown request id",
                Self::SessionMismatch => "browser response came from the wrong session",
                Self::Tab(_) => unreachable!("handled above"),
            }),
        }
    }
}

impl std::error::Error for RelayError {}

impl From<TabRegistryError> for RelayError {
    fn from(error: TabRegistryError) -> Self {
        Self::Tab(error)
    }
}

/// Native state for one active authenticated extension connection.
///
/// Replacing or losing a session fails every action owned by the prior session.
/// The transport must deliver those correlated failures back to waiting native
/// calls; ordinary TinyFlows retry policies decide whether another attempt runs.
#[derive(Debug)]
pub struct RelayState {
    policy: RelayPolicy,
    connected: Option<SessionId>,
    last_heartbeat: Option<Instant>,
    tabs: TabRegistry,
    pending: HashMap<String, PendingAction>,
}

impl RelayState {
    /// Creates disconnected relay state after validating loopback/deadline policy.
    pub fn new(policy: RelayPolicy) -> Result<Self, RelayError> {
        policy.validate()?;
        Ok(Self {
            policy,
            connected: None,
            last_heartbeat: None,
            tabs: TabRegistry::new(),
            pending: HashMap::new(),
        })
    }

    /// Returns the explicit shared-tab registry.
    pub fn tabs(&self) -> &TabRegistry {
        &self.tabs
    }

    /// Returns mutable access for authenticated share/revoke messages.
    pub fn tabs_mut(&mut self) -> &mut TabRegistry {
        &mut self.tabs
    }

    /// Installs an authenticated socket as the sole active extension session.
    ///
    /// The caller must first disconnect an existing session so interrupted
    /// actions receive deterministic errors rather than being silently adopted.
    pub fn connect(&mut self, session_id: SessionId, now: Instant) -> Result<(), RelayError> {
        if self.connected.is_some() {
            return Err(RelayError::SessionMismatch);
        }
        self.connected = Some(session_id);
        self.last_heartbeat = Some(now);
        Ok(())
    }

    /// Records an authenticated heartbeat from the current session.
    pub fn heartbeat(&mut self, session_id: &str, now: Instant) -> Result<(), RelayError> {
        if self.connected.as_deref() != Some(session_id) {
            return Err(RelayError::SessionMismatch);
        }
        self.last_heartbeat = Some(now);
        Ok(())
    }

    /// Registers a validated, explicitly bound action before sending it.
    pub fn begin_action(
        &mut self,
        request: &BrowserRequest,
        now: Instant,
    ) -> Result<&PendingAction, RelayError> {
        if request.protocol_version != BROWSER_PROTOCOL_VERSION {
            return Err(RelayError::ProtocolMismatch);
        }
        let session_id = self
            .connected
            .as_ref()
            .ok_or(RelayError::RelayDisconnected)?
            .clone();
        self.tabs.authorize(&request.run_id, request.tab_id)?;
        let timeout = Duration::from_millis(request.timeout_ms);
        if timeout.is_zero() || timeout > self.policy.maximum_action_timeout {
            return Err(RelayError::InvalidTimeout);
        }
        if self.pending.contains_key(&request.request_id) {
            return Err(RelayError::DuplicateRequestId);
        }
        self.pending.insert(
            request.request_id.clone(),
            PendingAction {
                request_id: request.request_id.clone(),
                run_id: request.run_id.clone(),
                tab_id: request.tab_id,
                session_id,
                deadline: now + timeout,
            },
        );
        Ok(self
            .pending
            .get(&request.request_id)
            .expect("pending action was just inserted"))
    }

    /// Accepts a terminal response only from the owning authenticated session.
    pub fn complete_action(
        &mut self,
        session_id: &str,
        response: &BrowserResponse,
    ) -> Result<PendingAction, RelayError> {
        if response.protocol_version() != BROWSER_PROTOCOL_VERSION {
            return Err(RelayError::ProtocolMismatch);
        }
        let pending = self
            .pending
            .get(response.request_id())
            .ok_or(RelayError::UnknownRequestId)?;
        if pending.session_id != session_id || self.connected.as_deref() != Some(session_id) {
            return Err(RelayError::SessionMismatch);
        }
        Ok(self
            .pending
            .remove(response.request_id())
            .expect("pending action was checked above"))
    }

    /// Fails and removes actions whose bounded deadlines elapsed.
    pub fn expire_actions(&mut self, now: Instant) -> Vec<BrowserResponse> {
        let mut expired = self
            .pending
            .values()
            .filter(|action| action.deadline <= now)
            .map(|action| action.request_id.clone())
            .collect::<Vec<_>>();
        expired.sort();
        expired
            .into_iter()
            .filter_map(|request_id| {
                self.pending.remove(&request_id)?;
                Some(error_response(
                    request_id,
                    BrowserErrorCode::ActionTimeout,
                    "browser action exceeded its deadline",
                ))
            })
            .collect()
    }

    /// Disconnects a stale heartbeat and fails every interrupted action.
    pub fn disconnect_if_stale(&mut self, now: Instant) -> Option<DisconnectOutcome> {
        let last = self.last_heartbeat?;
        if now.saturating_duration_since(last) < self.policy.heartbeat_timeout {
            return None;
        }
        let session_id = self.connected.clone()?;
        Some(self.disconnect(&session_id))
    }

    /// Disconnects the current authenticated session and fails closed.
    pub fn disconnect(&mut self, session_id: &str) -> DisconnectOutcome {
        if self.connected.as_deref() != Some(session_id) {
            return DisconnectOutcome {
                event: BrowserEvent::RelayDisconnected {
                    protocol_version: BROWSER_PROTOCOL_VERSION,
                },
                responses: Vec::new(),
            };
        }
        self.connected = None;
        self.last_heartbeat = None;
        let mut request_ids = self.pending.keys().cloned().collect::<Vec<_>>();
        request_ids.sort();
        let responses = request_ids
            .into_iter()
            .filter_map(|request_id| {
                self.pending.remove(&request_id)?;
                Some(error_response(
                    request_id,
                    BrowserErrorCode::RelayDisconnected,
                    "authenticated extension relay disconnected",
                ))
            })
            .collect();
        DisconnectOutcome {
            event: BrowserEvent::RelayDisconnected {
                protocol_version: BROWSER_PROTOCOL_VERSION,
            },
            responses,
        }
    }

    /// Revokes a tab and fails actions targeting it immediately.
    pub fn revoke_tab(&mut self, tab_id: TabId) -> (BrowserEvent, Vec<BrowserResponse>) {
        self.tabs.revoke(tab_id);
        let mut request_ids = self
            .pending
            .values()
            .filter(|pending| pending.tab_id == tab_id)
            .map(|pending| pending.request_id.clone())
            .collect::<Vec<_>>();
        request_ids.sort();
        let responses = request_ids
            .into_iter()
            .filter_map(|request_id| {
                self.pending.remove(&request_id)?;
                Some(error_response(
                    request_id,
                    BrowserErrorCode::TabRevoked,
                    "user revoked access to the shared tab",
                ))
            })
            .collect();
        (
            BrowserEvent::TabRevoked {
                protocol_version: BROWSER_PROTOCOL_VERSION,
                tab_id,
            },
            responses,
        )
    }

    /// Cancels all in-flight browser work for a native workflow run.
    pub fn cancel_run(&mut self, run_id: &str) -> Vec<BrowserResponse> {
        self.tabs.unbind_run(run_id);
        let mut request_ids = self
            .pending
            .values()
            .filter(|pending| pending.run_id == run_id)
            .map(|pending| pending.request_id.clone())
            .collect::<Vec<_>>();
        request_ids.sort();
        request_ids
            .into_iter()
            .filter_map(|request_id| {
                self.pending.remove(&request_id)?;
                Some(error_response(
                    request_id,
                    BrowserErrorCode::Cancelled,
                    "workflow run cancelled the browser action",
                ))
            })
            .collect()
    }

    /// Returns whether an authenticated extension session is live.
    pub fn is_connected(&self) -> bool {
        self.connected.is_some()
    }

    /// Returns the number of in-flight actions, primarily for host diagnostics.
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }
}

fn error_response(request_id: String, code: BrowserErrorCode, message: &str) -> BrowserResponse {
    BrowserResponse::Error {
        protocol_version: BROWSER_PROTOCOL_VERSION,
        request_id,
        error: BrowserError {
            code,
            message: message.to_owned(),
            details: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::BrowserAction;

    fn request(id: &str, run_id: &str, tab_id: TabId, timeout_ms: u64) -> BrowserRequest {
        BrowserRequest {
            protocol_version: BROWSER_PROTOCOL_VERSION,
            request_id: id.into(),
            run_id: run_id.into(),
            tab_id,
            timeout_ms,
            action: BrowserAction::GetTitle,
        }
    }

    fn connected_state(now: Instant) -> RelayState {
        let mut state = RelayState::new(RelayPolicy::loopback(3210)).unwrap();
        state
            .tabs_mut()
            .share(7, 1, "https://example.test", "Example")
            .unwrap();
        state.tabs_mut().bind_run("run-1", 7).unwrap();
        state.connect("session-1".into(), now).unwrap();
        state
    }

    #[test]
    fn policy_rejects_non_loopback_listener() {
        let mut policy = RelayPolicy::loopback(3210);
        policy.bind_addr = "0.0.0.0:3210".parse().unwrap();
        assert_eq!(policy.validate(), Err(RelayError::NonLoopbackBind));
    }

    #[test]
    fn action_requires_connection_binding_and_bounded_timeout() {
        let now = Instant::now();
        let mut disconnected = RelayState::new(RelayPolicy::loopback(3210)).unwrap();
        assert_eq!(
            disconnected
                .begin_action(&request("r0", "run-1", 7, 1000), now)
                .unwrap_err(),
            RelayError::RelayDisconnected
        );

        let mut state = connected_state(now);
        assert!(
            state
                .begin_action(&request("r1", "run-1", 7, 1000), now)
                .is_ok()
        );
        assert_eq!(
            state
                .begin_action(&request("r2", "run-1", 8, 1000), now)
                .unwrap_err(),
            RelayError::Tab(TabRegistryError::RunTabMismatch)
        );
        assert_eq!(
            state
                .begin_action(&request("r3", "run-1", 7, 60_001), now)
                .unwrap_err(),
            RelayError::InvalidTimeout
        );
    }

    #[test]
    fn timeout_disconnect_revocation_and_cancel_all_fail_closed() {
        let now = Instant::now();
        let mut state = connected_state(now);
        state
            .begin_action(&request("timeout", "run-1", 7, 100), now)
            .unwrap();
        let expired = state.expire_actions(now + Duration::from_millis(100));
        assert!(matches!(
            &expired[0],
            BrowserResponse::Error { error, .. }
                if error.code == BrowserErrorCode::ActionTimeout
        ));

        state
            .begin_action(&request("revoked", "run-1", 7, 1000), now)
            .unwrap();
        let (_, revoked) = state.revoke_tab(7);
        assert!(matches!(
            &revoked[0],
            BrowserResponse::Error { error, .. }
                if error.code == BrowserErrorCode::TabRevoked
        ));

        state
            .tabs_mut()
            .share(7, 1, "https://example.test", "Example")
            .unwrap();
        state.tabs_mut().bind_run("run-2", 7).unwrap();
        state
            .begin_action(&request("cancelled", "run-2", 7, 1000), now)
            .unwrap();
        let cancelled = state.cancel_run("run-2");
        assert!(matches!(
            &cancelled[0],
            BrowserResponse::Error { error, .. }
                if error.code == BrowserErrorCode::Cancelled
        ));

        state.tabs_mut().bind_run("run-3", 7).unwrap();
        state
            .begin_action(&request("disconnect", "run-3", 7, 1000), now)
            .unwrap();
        let disconnected = state.disconnect("session-1");
        assert!(matches!(
            &disconnected.responses[0],
            BrowserResponse::Error { error, .. }
                if error.code == BrowserErrorCode::RelayDisconnected
        ));
        assert!(!state.is_connected());
    }

    #[test]
    fn response_must_match_protocol_request_and_session() {
        let now = Instant::now();
        let mut state = connected_state(now);
        state
            .begin_action(&request("r1", "run-1", 7, 1000), now)
            .unwrap();
        let response = error_response("r1".into(), BrowserErrorCode::ElementNotFound, "not found");
        assert_eq!(
            state.complete_action("session-2", &response).unwrap_err(),
            RelayError::SessionMismatch
        );
        assert_eq!(
            state
                .complete_action("session-1", &response)
                .unwrap()
                .request_id,
            "r1"
        );
    }

    #[test]
    fn heartbeat_refreshes_liveness_and_stale_session_fails_actions() {
        let now = Instant::now();
        let mut state = connected_state(now);
        state
            .begin_action(&request("pending", "run-1", 7, 60_000), now)
            .unwrap();

        let refreshed = now + Duration::from_secs(20);
        state.heartbeat("session-1", refreshed).unwrap();
        assert!(
            state
                .disconnect_if_stale(now + Duration::from_secs(40))
                .is_none()
        );

        let outcome = state
            .disconnect_if_stale(now + Duration::from_secs(50))
            .expect("thirty seconds without a heartbeat is stale");
        assert!(matches!(
            &outcome.responses[0],
            BrowserResponse::Error { error, .. }
                if error.code == BrowserErrorCode::RelayDisconnected
        ));
        assert!(!state.is_connected());
    }
}
