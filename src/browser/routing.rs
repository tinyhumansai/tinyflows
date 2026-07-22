//! Browser relay adapter and deterministic tool routing.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use async_trait::async_trait;
use serde_json::Value;

use super::protocol::{
    BROWSER_PROTOCOL_VERSION, BrowserAction, BrowserError, BrowserErrorCode, BrowserRequest,
    BrowserResponse,
};
use crate::caps::ToolInvoker;
use crate::error::{EngineError, Result};

/// Default deadline attached to a browser action when the host does not override it.
pub const DEFAULT_BROWSER_ACTION_TIMEOUT_MS: u64 = 30_000;

/// Authenticated transport from the native companion to the Chrome extension.
///
/// Implementations own WebSocket authentication, connection health, and tab
/// registration. Returning an error fails the workflow step closed; TinyFlows'
/// ordinary node retry and error-port policies then decide what happens next.
#[async_trait]
pub trait BrowserRelay: Send + Sync {
    /// Sends one correlated request and waits for its terminal response.
    async fn execute(
        &self,
        request: BrowserRequest,
    ) -> std::result::Result<BrowserResponse, BrowserError>;
}

/// A [`ToolInvoker`] that turns `browser` tool calls into Chrome relay requests.
///
/// Each instance is bound to exactly one workflow run and one explicitly shared
/// tab. There is no fallback tab selection. Construct a fresh instance when a
/// side panel or CLI run selects its owning tab.
pub struct ChromeToolInvoker {
    relay: Arc<dyn BrowserRelay>,
    run_id: String,
    tab_id: u64,
    timeout_ms: u64,
    sequence: AtomicU64,
}

impl ChromeToolInvoker {
    /// Creates an invoker bound to `run_id` and the explicitly shared `tab_id`.
    pub fn new(relay: Arc<dyn BrowserRelay>, run_id: impl Into<String>, tab_id: u64) -> Self {
        Self {
            relay,
            run_id: run_id.into(),
            tab_id,
            timeout_ms: DEFAULT_BROWSER_ACTION_TIMEOUT_MS,
            sequence: AtomicU64::new(0),
        }
    }

    /// Overrides the bounded per-action relay deadline.
    pub fn with_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = timeout_ms;
        self
    }

    fn next_request_id(&self) -> String {
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed) + 1;
        format!("{}:{sequence}", self.run_id)
    }

    fn capability_error(error: &BrowserError) -> EngineError {
        EngineError::Capability(format!(
            "browser:{}: {}",
            error.code.as_str(),
            error.message
        ))
    }
}

#[async_trait]
impl ToolInvoker for ChromeToolInvoker {
    async fn invoke(&self, slug: &str, args: Value, _conn: Option<&str>) -> Result<Value> {
        if slug != "browser" {
            return Err(Self::capability_error(&BrowserError {
                code: BrowserErrorCode::InvalidRequest,
                message: format!("ChromeToolInvoker only accepts slug `browser`, got `{slug}`"),
                details: None,
            }));
        }

        let action: BrowserAction = serde_json::from_value(args).map_err(|error| {
            Self::capability_error(&BrowserError {
                code: BrowserErrorCode::InvalidRequest,
                message: format!("invalid browser args: {error}"),
                details: None,
            })
        })?;
        let request_id = self.next_request_id();
        let request = BrowserRequest {
            protocol_version: BROWSER_PROTOCOL_VERSION,
            request_id: request_id.clone(),
            run_id: self.run_id.clone(),
            tab_id: self.tab_id,
            timeout_ms: self.timeout_ms,
            action,
        };

        let response = self
            .relay
            .execute(request)
            .await
            .map_err(|error| Self::capability_error(&error))?;

        if response.protocol_version() != BROWSER_PROTOCOL_VERSION {
            return Err(Self::capability_error(&BrowserError {
                code: BrowserErrorCode::ProtocolMismatch,
                message: format!(
                    "relay responded with protocol version {}, expected {}",
                    response.protocol_version(),
                    BROWSER_PROTOCOL_VERSION
                ),
                details: None,
            }));
        }
        if response.request_id() != request_id {
            return Err(Self::capability_error(&BrowserError {
                code: BrowserErrorCode::InvalidRequest,
                message: format!(
                    "relay response correlation mismatch: expected `{request_id}`, got `{}`",
                    response.request_id()
                ),
                details: None,
            }));
        }

        match response {
            BrowserResponse::Ok { result, .. } => Ok(result.data),
            BrowserResponse::Error { error, .. } => Err(Self::capability_error(&error)),
        }
    }
}

/// Routes explicit browser calls to Chrome and delegates every other tool unchanged.
pub struct RoutingToolInvoker {
    browser: Arc<ChromeToolInvoker>,
    fallback: Arc<dyn ToolInvoker>,
}

impl RoutingToolInvoker {
    /// Creates a router around one run/tab-bound browser invoker and a host invoker.
    pub fn new(browser: Arc<ChromeToolInvoker>, fallback: Arc<dyn ToolInvoker>) -> Self {
        Self { browser, fallback }
    }
}

#[async_trait]
impl ToolInvoker for RoutingToolInvoker {
    async fn invoke(&self, slug: &str, args: Value, conn: Option<&str>) -> Result<Value> {
        if slug == "browser" {
            self.browser.invoke(slug, args, conn).await
        } else {
            self.fallback.invoke(slug, args, conn).await
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use serde_json::json;

    use super::*;
    use crate::error::EngineError;

    struct RecordingRelay {
        requests: Mutex<Vec<BrowserRequest>>,
        response: Mutex<Option<std::result::Result<BrowserResponse, BrowserError>>>,
    }

    impl RecordingRelay {
        fn success(data: Value) -> Arc<Self> {
            Arc::new(Self {
                requests: Mutex::new(Vec::new()),
                response: Mutex::new(Some(Ok(BrowserResponse::Ok {
                    protocol_version: BROWSER_PROTOCOL_VERSION,
                    request_id: String::new(),
                    result: super::super::protocol::BrowserResult { data },
                }))),
            })
        }
    }

    #[async_trait]
    impl BrowserRelay for RecordingRelay {
        async fn execute(
            &self,
            request: BrowserRequest,
        ) -> std::result::Result<BrowserResponse, BrowserError> {
            self.requests.lock().unwrap().push(request.clone());
            let response = self.response.lock().unwrap().take().unwrap();
            match response {
                Ok(BrowserResponse::Ok {
                    protocol_version,
                    result,
                    ..
                }) => Ok(BrowserResponse::Ok {
                    protocol_version,
                    request_id: request.request_id,
                    result,
                }),
                other => other,
            }
        }
    }

    #[derive(Default)]
    struct RecordingFallback {
        calls: Mutex<Vec<(String, Value, Option<String>)>>,
    }

    #[async_trait]
    impl ToolInvoker for RecordingFallback {
        async fn invoke(&self, slug: &str, args: Value, conn: Option<&str>) -> Result<Value> {
            self.calls.lock().unwrap().push((
                slug.to_owned(),
                args.clone(),
                conn.map(str::to_owned),
            ));
            Ok(json!({"fallback": slug, "args": args}))
        }
    }

    #[tokio::test]
    async fn chrome_invoker_requires_an_explicit_action() {
        let relay = RecordingRelay::success(Value::Null);
        let invoker = ChromeToolInvoker::new(relay, "run-1", 7);
        let error = invoker
            .invoke("browser", json!({"selector":"main"}), None)
            .await
            .expect_err("missing args.action must fail");
        assert!(
            matches!(error, EngineError::Capability(message) if message.starts_with("browser:invalid_request:"))
        );
    }

    #[tokio::test]
    async fn chrome_invoker_binds_run_tab_timeout_and_correlation() {
        let relay = RecordingRelay::success(json!({"title":"Example"}));
        let invoker = ChromeToolInvoker::new(relay.clone(), "run-42", 91).with_timeout_ms(1234);

        let output = invoker
            .invoke("browser", json!({"action":"get_title"}), None)
            .await
            .expect("browser action");
        assert_eq!(output, json!({"title":"Example"}));
        let requests = relay.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].protocol_version, BROWSER_PROTOCOL_VERSION);
        assert_eq!(requests[0].request_id, "run-42:1");
        assert_eq!(requests[0].run_id, "run-42");
        assert_eq!(requests[0].tab_id, 91);
        assert_eq!(requests[0].timeout_ms, 1234);
        assert_eq!(requests[0].action, BrowserAction::GetTitle);
    }

    #[tokio::test]
    async fn router_delegates_non_browser_slug_args_and_connection_unchanged() {
        let relay = RecordingRelay::success(Value::Null);
        let browser = Arc::new(ChromeToolInvoker::new(relay.clone(), "run", 1));
        let fallback = Arc::new(RecordingFallback::default());
        let router = RoutingToolInvoker::new(browser, fallback.clone());
        let args = json!({"to":"person@example.com","body":"hello"});

        let output = router
            .invoke("gmail.send", args.clone(), Some("acct-9"))
            .await
            .expect("fallback action");
        assert_eq!(output["fallback"], "gmail.send");
        assert!(relay.requests.lock().unwrap().is_empty());
        assert_eq!(
            fallback.calls.lock().unwrap().as_slice(),
            &[("gmail.send".into(), args, Some("acct-9".into()))]
        );
    }

    #[tokio::test]
    async fn stable_relay_error_code_reaches_engine_retry_surface() {
        let relay = Arc::new(RecordingRelay {
            requests: Mutex::new(Vec::new()),
            response: Mutex::new(Some(Err(BrowserError {
                code: BrowserErrorCode::TabRevoked,
                message: "user removed tab from TinyFlows group".into(),
                details: None,
            }))),
        });
        let invoker = ChromeToolInvoker::new(relay, "run", 1);

        let error = invoker
            .invoke("browser", json!({"action":"snapshot"}), None)
            .await
            .expect_err("revoked tab must fail closed");
        assert!(
            matches!(error, EngineError::Capability(message) if message.starts_with("browser:tab_revoked:"))
        );
    }

    #[tokio::test]
    async fn mismatched_response_correlation_fails_closed() {
        struct WrongCorrelation;
        #[async_trait]
        impl BrowserRelay for WrongCorrelation {
            async fn execute(
                &self,
                _request: BrowserRequest,
            ) -> std::result::Result<BrowserResponse, BrowserError> {
                Ok(BrowserResponse::Ok {
                    protocol_version: BROWSER_PROTOCOL_VERSION,
                    request_id: "another-run:99".into(),
                    result: super::super::protocol::BrowserResult { data: Value::Null },
                })
            }
        }

        let invoker = ChromeToolInvoker::new(Arc::new(WrongCorrelation), "run", 1);
        let error = invoker
            .invoke("browser", json!({"action":"get_url"}), None)
            .await
            .expect_err("cross-run response must fail");
        assert!(
            matches!(error, EngineError::Capability(message) if message.starts_with("browser:invalid_request:"))
        );
    }
}
