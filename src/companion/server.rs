//! Authenticated loopback WebSocket adapter and native workflow runner.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::Router;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Json, State, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use crate::browser::{
    BROWSER_PROTOCOL_VERSION, BrowserCancel, BrowserCancelType, BrowserError, BrowserErrorCode,
    BrowserEvent, BrowserRelay, BrowserRequest, BrowserResponse, ChromeToolInvoker,
    RoutingToolInvoker,
};
use crate::caps::Capabilities;
use crate::compiler::compile;
use crate::engine::{CancellationToken, run_cancellable_with_observer};
use crate::model::WorkflowGraph;
use crate::observability::{ExecutionStep, RunObserver};

use super::{
    Authenticator, CompanionControlRequest, CompanionControlResponse, PROTOCOL_SUBPROTOCOL,
    PairingSecret, RelayPolicy, RelayState, RunEvent, TabId, WebSocketHandshake, WorkflowSummary,
};

/// Configuration for the native Chrome companion.
#[derive(Clone)]
pub struct CompanionServerConfig {
    /// Loopback/deadline policy for the relay listener.
    pub policy: RelayPolicy,
    /// Exact Chrome extension id allowed by the WebSocket origin check.
    pub extension_id: String,
    /// Host-local pairing secret required in a WebSocket subprotocol.
    pub pairing_secret: PairingSecret,
    /// Directory containing workflow JSON files exposed to the side panel.
    pub workflows_dir: PathBuf,
    /// Host capabilities used for every non-browser effect.
    pub capabilities: Capabilities,
}

/// Errors produced by companion configuration, I/O, or workflow startup.
#[derive(Debug, thiserror::Error)]
pub enum CompanionServerError {
    /// Relay or tab policy rejected an operation.
    #[error("relay policy error: {0}")]
    Relay(#[from] super::RelayError),
    /// Pairing or extension-id configuration was invalid.
    #[error("authentication configuration error: {0}")]
    Authentication(#[from] std::io::Error),
    /// The loopback listener failed.
    #[error("listener error: {0}")]
    Listener(String),
    /// A workflow file could not be read, validated, or decoded.
    #[error("workflow error: {0}")]
    Workflow(String),
}

type PendingSender = oneshot::Sender<std::result::Result<BrowserResponse, BrowserError>>;

struct ServerInner {
    bind_addr: SocketAddr,
    native_secret: String,
    authenticator: Authenticator,
    relay: Mutex<RelayState>,
    outbound: Mutex<Option<mpsc::UnboundedSender<Message>>>,
    pending: tokio::sync::Mutex<HashMap<String, PendingSender>>,
    workflows_dir: PathBuf,
    capabilities: Capabilities,
    runs: Mutex<HashMap<String, CancellationToken>>,
    next_session: AtomicU64,
    next_run: AtomicU64,
}

/// Loopback-only native companion used by the TinyFlows Chrome extension.
#[derive(Clone)]
pub struct CompanionServer {
    inner: Arc<ServerInner>,
}

impl CompanionServer {
    /// Validates configuration and creates a disconnected server.
    pub fn new(config: CompanionServerConfig) -> Result<Self, CompanionServerError> {
        config.policy.validate()?;
        let bind_addr = config.policy.bind_addr;
        let native_secret = config.pairing_secret.expose().to_owned();
        let authenticator = Authenticator::new(&config.extension_id, config.pairing_secret)?;
        Ok(Self {
            inner: Arc::new(ServerInner {
                bind_addr,
                native_secret,
                authenticator,
                relay: Mutex::new(RelayState::new(config.policy)?),
                outbound: Mutex::new(None),
                pending: tokio::sync::Mutex::new(HashMap::new()),
                workflows_dir: config.workflows_dir,
                capabilities: config.capabilities,
                runs: Mutex::new(HashMap::new()),
                next_session: AtomicU64::new(0),
                next_run: AtomicU64::new(0),
            }),
        })
    }

    /// Returns the loopback socket address the server will bind.
    pub fn bind_addr(&self) -> SocketAddr {
        self.inner.bind_addr
    }

    /// Runs the authenticated WebSocket endpoint until listener failure.
    pub async fn serve(self) -> Result<(), CompanionServerError> {
        let listener = tokio::net::TcpListener::bind(self.inner.bind_addr)
            .await
            .map_err(|error| CompanionServerError::Listener(error.to_string()))?;
        let app = Router::new()
            .route("/v1/extension", get(upgrade))
            .route("/v1/native/tabs", get(native_tabs))
            .route("/v1/native/workflows", get(native_workflows))
            .route("/v1/native/runs", post(native_run))
            .with_state(self);
        axum::serve(listener, app)
            .await
            .map_err(|error| CompanionServerError::Listener(error.to_string()))
    }

    /// Lists valid workflow JSON files from the configured directory.
    pub fn workflows(&self) -> Result<Vec<WorkflowSummary>, CompanionServerError> {
        list_workflows(&self.inner.workflows_dir)
    }

    /// Starts a native run bound to one explicit shared tab.
    pub async fn start_workflow(
        &self,
        workflow_id: &str,
        tab_id: TabId,
        input: Value,
    ) -> Result<String, CompanionServerError> {
        let graph = load_workflow(&self.inner.workflows_dir, workflow_id)?;
        let node_kinds = graph
            .nodes
            .iter()
            .map(|node| {
                let kind = serde_json::to_value(&node.kind)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_owned))
                    .unwrap_or_else(|| "unknown".into());
                (node.id.clone(), kind)
            })
            .collect::<HashMap<_, _>>();
        let compiled =
            compile(&graph).map_err(|error| CompanionServerError::Workflow(error.to_string()))?;
        let sequence = self.inner.next_run.fetch_add(1, Ordering::Relaxed) + 1;
        let run_id = format!("chrome-run-{sequence}");
        self.inner
            .relay
            .lock()
            .map_err(|_| lock_error())?
            .tabs_mut()
            .bind_run(run_id.clone(), tab_id)
            .map_err(super::RelayError::from)?;

        let token = CancellationToken::new();
        self.inner
            .runs
            .lock()
            .map_err(|_| lock_error())?
            .insert(run_id.clone(), token.clone());
        let server = self.clone();
        let spawned_run_id = run_id.clone();
        tokio::spawn(async move {
            server.send_json(&RunEvent::Started {
                protocol_version: BROWSER_PROTOCOL_VERSION,
                run_id: spawned_run_id.clone(),
                tab_id,
            });
            let browser = Arc::new(ChromeToolInvoker::new(
                Arc::new(SocketRelay {
                    inner: server.inner.clone(),
                }),
                spawned_run_id.clone(),
                tab_id,
            ));
            let mut capabilities = server.inner.capabilities.clone();
            capabilities.tools = Arc::new(RoutingToolInvoker::new(
                browser,
                server.inner.capabilities.tools.clone(),
            ));
            let observer = Arc::new(CompanionObserver {
                server: server.clone(),
                run_id: spawned_run_id.clone(),
                node_kinds,
            }) as Arc<dyn RunObserver>;
            match run_cancellable_with_observer(&compiled, input, &capabilities, token, &observer)
                .await
            {
                Ok(value) if value.cancelled => server.send_json(&RunEvent::Cancelled {
                    protocol_version: BROWSER_PROTOCOL_VERSION,
                    run_id: spawned_run_id.clone(),
                }),
                Ok(value) if !value.pending_approvals.is_empty() => {
                    server.send_json(&RunEvent::AwaitingApproval {
                        protocol_version: BROWSER_PROTOCOL_VERSION,
                        run_id: spawned_run_id.clone(),
                        pending_approvals: value.pending_approvals,
                    });
                }
                Ok(value) => server.send_json(&RunEvent::Completed {
                    protocol_version: BROWSER_PROTOCOL_VERSION,
                    run_id: spawned_run_id.clone(),
                    output: value.output,
                }),
                Err(error) => server.send_json(&RunEvent::Failed {
                    protocol_version: BROWSER_PROTOCOL_VERSION,
                    run_id: spawned_run_id.clone(),
                    code: "workflow_failed".into(),
                    message: error.to_string(),
                }),
            }
            if let Ok(mut relay) = server.inner.relay.lock() {
                relay.tabs_mut().unbind_run(&spawned_run_id);
            }
            if let Ok(mut runs) = server.inner.runs.lock() {
                runs.remove(&spawned_run_id);
            }
        });
        Ok(run_id)
    }

    /// Cancels a live run and its in-flight browser action.
    pub async fn cancel_workflow(&self, run_id: &str) -> bool {
        let token = self
            .inner
            .runs
            .lock()
            .ok()
            .and_then(|runs| runs.get(run_id).cloned());
        let Some(token) = token else { return false };
        token.cancel();
        let responses = self
            .inner
            .relay
            .lock()
            .map(|mut relay| relay.cancel_run(run_id))
            .unwrap_or_default();
        self.dispatch(responses).await;
        true
    }

    fn send_json<T: serde::Serialize>(&self, value: &T) {
        let Ok(text) = serde_json::to_string(value) else {
            return;
        };
        if let Ok(outbound) = self.inner.outbound.lock()
            && let Some(sender) = outbound.as_ref()
        {
            let _ = sender.send(Message::Text(text.into()));
        }
    }

    async fn dispatch(&self, responses: Vec<BrowserResponse>) {
        let mut pending = self.inner.pending.lock().await;
        for response in responses {
            if matches!(response, BrowserResponse::Error { .. }) {
                self.send_json(&BrowserCancel {
                    protocol_version: BROWSER_PROTOCOL_VERSION,
                    message_type: BrowserCancelType::BrowserCancel,
                    request_id: response.request_id().to_owned(),
                });
            }
            if let Some(sender) = pending.remove(response.request_id()) {
                let result = match response {
                    BrowserResponse::Error { error, .. } => Err(error),
                    success => Ok(success),
                };
                let _ = sender.send(result);
            }
        }
    }
}

struct CompanionObserver {
    server: CompanionServer,
    run_id: String,
    node_kinds: HashMap<String, String>,
}

impl RunObserver for CompanionObserver {
    fn on_step_start(&self, node_id: &str) {
        let node_kind = self
            .node_kinds
            .get(node_id)
            .cloned()
            .unwrap_or_else(|| "unknown".into());
        self.server.send_json(&RunEvent::StepStarted {
            protocol_version: BROWSER_PROTOCOL_VERSION,
            run_id: self.run_id.clone(),
            node_id: node_id.to_owned(),
            node_kind,
        });
    }

    fn on_step_finish(&self, step: &ExecutionStep) {
        let node_kind = self
            .node_kinds
            .get(&step.node_id)
            .cloned()
            .unwrap_or_else(|| "unknown".into());
        self.server.send_json(&RunEvent::StepCompleted {
            protocol_version: BROWSER_PROTOCOL_VERSION,
            run_id: self.run_id.clone(),
            node_id: step.node_id.clone(),
            node_kind,
            output: step.output.clone(),
        });
    }
}

struct SocketRelay {
    inner: Arc<ServerInner>,
}

#[async_trait]
impl BrowserRelay for SocketRelay {
    async fn execute(
        &self,
        request: BrowserRequest,
    ) -> std::result::Result<BrowserResponse, BrowserError> {
        self.inner
            .relay
            .lock()
            .map_err(|_| browser_error(BrowserErrorCode::RelayDisconnected, "relay unavailable"))?
            .begin_action(&request, Instant::now())
            .map_err(|error| browser_error(error.browser_code(), &error.to_string()))?;
        let (sender, receiver) = oneshot::channel();
        self.inner
            .pending
            .lock()
            .await
            .insert(request.request_id.clone(), sender);
        let wire = serde_json::to_string(&request)
            .map_err(|error| browser_error(BrowserErrorCode::InvalidRequest, &error.to_string()))?;
        let outbound = self
            .inner
            .outbound
            .lock()
            .map_err(|_| browser_error(BrowserErrorCode::RelayDisconnected, "relay unavailable"))?
            .clone()
            .ok_or_else(|| {
                browser_error(
                    BrowserErrorCode::RelayDisconnected,
                    "extension is disconnected",
                )
            })?;
        if outbound.send(Message::Text(wire.into())).is_err() {
            self.inner.pending.lock().await.remove(&request.request_id);
            return Err(browser_error(
                BrowserErrorCode::RelayDisconnected,
                "extension connection closed",
            ));
        }
        match tokio::time::timeout(Duration::from_millis(request.timeout_ms), receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(browser_error(
                BrowserErrorCode::RelayDisconnected,
                "relay response channel closed",
            )),
            Err(_) => {
                self.inner.pending.lock().await.remove(&request.request_id);
                if let Ok(mut relay) = self.inner.relay.lock() {
                    let _ = relay.expire_actions(Instant::now());
                }
                send_cancel(&self.inner, &request.request_id);
                Err(browser_error(
                    BrowserErrorCode::ActionTimeout,
                    "browser action exceeded its deadline",
                ))
            }
        }
    }
}

fn send_cancel(inner: &ServerInner, request_id: &str) {
    let cancel = BrowserCancel {
        protocol_version: BROWSER_PROTOCOL_VERSION,
        message_type: BrowserCancelType::BrowserCancel,
        request_id: request_id.to_owned(),
    };
    let Ok(wire) = serde_json::to_string(&cancel) else {
        return;
    };
    if let Ok(outbound) = inner.outbound.lock()
        && let Some(sender) = outbound.as_ref()
    {
        let _ = sender.send(Message::Text(wire.into()));
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeRunRequest {
    workflow_id: String,
    tab_id: TabId,
    #[serde(default)]
    input: Value,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct HeartbeatMessage {
    protocol_version: u32,
    #[serde(rename = "type")]
    message_type: HeartbeatType,
}

#[derive(serde::Deserialize)]
enum HeartbeatType {
    #[serde(rename = "heartbeat")]
    Heartbeat,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TabSharedMessage {
    protocol_version: u32,
    event: TabSharedType,
    tab: AnnouncedTab,
}

#[derive(serde::Deserialize)]
enum TabSharedType {
    #[serde(rename = "tab_shared")]
    TabShared,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct AnnouncedTab {
    id: u64,
    window_id: u64,
    url: String,
    title: String,
}

async fn native_tabs(State(server): State<CompanionServer>, headers: HeaderMap) -> Response {
    if !native_authorized(&server, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let tabs = server
        .inner
        .relay
        .lock()
        .map(|relay| relay.tabs().list().into_iter().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    Json(json!({"protocol_version":BROWSER_PROTOCOL_VERSION,"tabs":tabs})).into_response()
}

async fn native_workflows(State(server): State<CompanionServer>, headers: HeaderMap) -> Response {
    if !native_authorized(&server, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match server.workflows() {
        Ok(workflows) => Json(json!({
            "protocol_version":BROWSER_PROTOCOL_VERSION,
            "workflows":workflows
        }))
        .into_response(),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"code":"workflow_list_failed","message":error.to_string()})),
        )
            .into_response(),
    }
}

async fn native_run(
    State(server): State<CompanionServer>,
    headers: HeaderMap,
    Json(request): Json<NativeRunRequest>,
) -> Response {
    if !native_authorized(&server, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match server
        .start_workflow(&request.workflow_id, request.tab_id, request.input)
        .await
    {
        Ok(run_id) => Json(json!({
            "protocol_version":BROWSER_PROTOCOL_VERSION,
            "run_id":run_id
        }))
        .into_response(),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"code":"workflow_start_failed","message":error.to_string()})),
        )
            .into_response(),
    }
}

fn native_authorized(server: &CompanionServer, headers: &HeaderMap) -> bool {
    let candidate = header(headers, "authorization");
    let candidate = candidate.strip_prefix("Bearer ").unwrap_or_default();
    constant_time_eq(candidate.as_bytes(), server.inner.native_secret.as_bytes())
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    for index in 0..left.len().max(right.len()) {
        difference |= usize::from(
            left.get(index).copied().unwrap_or_default()
                ^ right.get(index).copied().unwrap_or_default(),
        );
    }
    difference == 0
}

async fn upgrade(
    State(server): State<CompanionServer>,
    headers: HeaderMap,
    websocket: WebSocketUpgrade,
) -> Response {
    let origin = header(&headers, "origin");
    let protocols_header = header(&headers, "sec-websocket-protocol");
    let offered = protocols_header
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    if server
        .inner
        .authenticator
        .authenticate(&WebSocketHandshake {
            origin: &origin,
            subprotocols: &offered,
        })
        .is_err()
    {
        return (StatusCode::UNAUTHORIZED, "unauthorized extension relay").into_response();
    }
    websocket
        .protocols([PROTOCOL_SUBPROTOCOL])
        .on_upgrade(move |socket| extension_session(server, socket))
}

async fn extension_session(server: CompanionServer, socket: WebSocket) {
    let session_id = format!(
        "extension-session-{}",
        server.inner.next_session.fetch_add(1, Ordering::Relaxed) + 1
    );
    let (mut sink, mut stream) = socket.split();
    let (sender, mut receiver) = mpsc::unbounded_channel::<Message>();
    {
        let Ok(mut relay) = server.inner.relay.lock() else {
            return;
        };
        if relay.is_connected() || relay.connect(session_id.clone(), Instant::now()).is_err() {
            return;
        }
    }
    if let Ok(mut outbound) = server.inner.outbound.lock() {
        *outbound = Some(sender);
    }
    let writer = tokio::spawn(async move {
        while let Some(message) = receiver.recv().await {
            if sink.send(message).await.is_err() {
                break;
            }
        }
    });
    let mut heartbeat_check = tokio::time::interval(Duration::from_secs(5));
    heartbeat_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    heartbeat_check.tick().await;
    loop {
        tokio::select! {
            message = stream.next() => match message {
                Some(Ok(Message::Text(text))) => {
                    if handle_text(&server, &session_id, &text).await.is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                _ => {}
            },
            _ = heartbeat_check.tick() => {
                let responses = {
                    server.inner.relay.lock().ok()
                        .and_then(|mut relay| relay.disconnect_if_stale(Instant::now()))
                        .map(|outcome| outcome.responses)
                };
                if let Some(responses) = responses {
                    server.dispatch(responses).await;
                    break;
                }
            },
        }
    }
    if let Ok(mut outbound) = server.inner.outbound.lock() {
        *outbound = None;
    }
    let responses = {
        server
            .inner
            .relay
            .lock()
            .map(|mut relay| relay.disconnect(&session_id).responses)
            .unwrap_or_default()
    };
    server.dispatch(responses).await;
    writer.abort();
}

async fn handle_text(server: &CompanionServer, session: &str, text: &str) -> Result<(), ()> {
    let value: Value = serde_json::from_str(text).map_err(|_| ())?;
    if let Ok(heartbeat) = serde_json::from_value::<HeartbeatMessage>(value.clone()) {
        if heartbeat.protocol_version != BROWSER_PROTOCOL_VERSION {
            return Err(());
        }
        let HeartbeatType::Heartbeat = heartbeat.message_type;
        return server
            .inner
            .relay
            .lock()
            .map_err(|_| ())?
            .heartbeat(session, Instant::now())
            .map_err(|_| ());
    }
    if let Ok(response) = serde_json::from_value::<BrowserResponse>(value.clone()) {
        let completion = server
            .inner
            .relay
            .lock()
            .map_err(|_| ())?
            .complete_action(session, &response);
        if matches!(completion, Err(super::RelayError::UnknownRequestId)) {
            return Ok(());
        }
        completion.map_err(|_| ())?;
        if let Some(sender) = server
            .inner
            .pending
            .lock()
            .await
            .remove(response.request_id())
        {
            let _ = sender.send(Ok(response));
        }
        return Ok(());
    }
    if let Ok(message) = serde_json::from_value::<TabSharedMessage>(value.clone()) {
        if message.protocol_version != BROWSER_PROTOCOL_VERSION {
            return Err(());
        }
        let TabSharedType::TabShared = message.event;
        server
            .inner
            .relay
            .lock()
            .map_err(|_| ())?
            .tabs_mut()
            .share(
                message.tab.id,
                message.tab.window_id,
                message.tab.url,
                message.tab.title,
            )
            .map_err(|_| ())?;
        return Ok(());
    }
    if let Ok(event) = serde_json::from_value::<BrowserEvent>(value.clone()) {
        let version = match &event {
            BrowserEvent::ActionStarted {
                protocol_version, ..
            }
            | BrowserEvent::ActionCompleted {
                protocol_version, ..
            }
            | BrowserEvent::ActionFailed {
                protocol_version, ..
            }
            | BrowserEvent::TabRevoked {
                protocol_version, ..
            }
            | BrowserEvent::RelayDisconnected { protocol_version } => *protocol_version,
        };
        if version != BROWSER_PROTOCOL_VERSION {
            return Err(());
        }
        match event {
            BrowserEvent::TabRevoked { tab_id, .. } => {
                let (_, responses) = server
                    .inner
                    .relay
                    .lock()
                    .map_err(|_| ())?
                    .revoke_tab(tab_id);
                server.dispatch(responses).await;
                return Ok(());
            }
            BrowserEvent::ActionStarted { .. }
            | BrowserEvent::ActionCompleted { .. }
            | BrowserEvent::ActionFailed { .. } => return Ok(()),
            BrowserEvent::RelayDisconnected { .. } => return Err(()),
        }
    }
    let request = serde_json::from_value::<CompanionControlRequest>(value).map_err(|_| ())?;
    let response = handle_control(server, request).await;
    server.send_json(&response);
    Ok(())
}

async fn handle_control(
    server: &CompanionServer,
    request: CompanionControlRequest,
) -> CompanionControlResponse {
    let request_id = request.request_id().to_owned();
    if request.protocol_version() != BROWSER_PROTOCOL_VERSION {
        return control_error(
            request_id,
            "protocol_mismatch",
            "unsupported control protocol",
        );
    }
    match request {
        CompanionControlRequest::WorkflowList { .. } => match server.workflows() {
            Ok(workflows) => CompanionControlResponse::Workflows {
                protocol_version: BROWSER_PROTOCOL_VERSION,
                request_id,
                workflows,
            },
            Err(error) => control_error(request_id, "workflow_list_failed", &error.to_string()),
        },
        CompanionControlRequest::WorkflowStart {
            workflow_id,
            tab_id,
            input,
            ..
        } => match server.start_workflow(&workflow_id, tab_id, input).await {
            Ok(run_id) => control_ok(request_id, json!({"run_id":run_id})),
            Err(error) => control_error(request_id, "workflow_start_failed", &error.to_string()),
        },
        CompanionControlRequest::WorkflowCancel { run_id, .. } => control_ok(
            request_id,
            json!({"cancelled":server.cancel_workflow(&run_id).await}),
        ),
        CompanionControlRequest::RunSubscribe { run_id, .. } => {
            control_ok(request_id, json!({"subscribed":run_id}))
        }
        CompanionControlRequest::TabList { .. } => {
            let tabs = server
                .inner
                .relay
                .lock()
                .map(|relay| relay.tabs().list().into_iter().cloned().collect())
                .unwrap_or_default();
            CompanionControlResponse::Tabs {
                protocol_version: BROWSER_PROTOCOL_VERSION,
                request_id,
                tabs,
            }
        }
        CompanionControlRequest::ConnectionStatus { .. } => {
            let connected = server
                .inner
                .relay
                .lock()
                .map(|relay| relay.is_connected())
                .unwrap_or(false);
            CompanionControlResponse::Connection {
                protocol_version: BROWSER_PROTOCOL_VERSION,
                request_id,
                connected,
            }
        }
    }
}

fn load_workflow(directory: &Path, id: &str) -> Result<WorkflowGraph, CompanionServerError> {
    if id.is_empty()
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(CompanionServerError::Workflow("invalid workflow id".into()));
    }
    let path = directory.join(format!("{id}.json"));
    let source = std::fs::read_to_string(&path)
        .map_err(|error| CompanionServerError::Workflow(format!("{}: {error}", path.display())))?;
    serde_json::from_str(&source)
        .map_err(|error| CompanionServerError::Workflow(format!("{}: {error}", path.display())))
}

fn list_workflows(directory: &Path) -> Result<Vec<WorkflowSummary>, CompanionServerError> {
    let entries = std::fs::read_dir(directory).map_err(|error| {
        CompanionServerError::Workflow(format!("{}: {error}", directory.display()))
    })?;
    let mut workflows = Vec::new();
    for entry in entries {
        let path = entry
            .map_err(|error| CompanionServerError::Workflow(error.to_string()))?
            .path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|value| value.to_str()) else {
            continue;
        };
        let graph = load_workflow(directory, id)?;
        workflows.push(WorkflowSummary {
            id: id.to_owned(),
            name: if graph.name.is_empty() {
                id.to_owned()
            } else {
                graph.name
            },
        });
    }
    workflows.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(workflows)
}

fn header(headers: &HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}

fn lock_error() -> CompanionServerError {
    CompanionServerError::Listener("companion state lock poisoned".into())
}

fn browser_error(code: BrowserErrorCode, message: &str) -> BrowserError {
    BrowserError {
        code,
        message: message.to_owned(),
        details: None,
    }
}

fn control_ok(request_id: String, result: Value) -> CompanionControlResponse {
    CompanionControlResponse::Ok {
        protocol_version: BROWSER_PROTOCOL_VERSION,
        request_id,
        result,
    }
}

fn control_error(request_id: String, code: &str, message: &str) -> CompanionControlResponse {
    CompanionControlResponse::Error {
        protocol_version: BROWSER_PROTOCOL_VERSION,
        request_id,
        code: code.to_owned(),
        message: message.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_ids_cannot_escape_the_configured_directory() {
        let error = load_workflow(Path::new("/tmp"), "../secret").unwrap_err();
        assert!(error.to_string().contains("invalid workflow id"));
    }
}
